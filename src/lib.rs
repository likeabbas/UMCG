#![allow(warnings)]
use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};
use std::collections::{VecDeque, HashMap, HashSet};
use uuid::Uuid;
use log::error;

static DEBUG_LOGGING: bool = true;

// Logging macros - MUST BE DEFINED BEFORE USE
macro_rules! info {
    ($($arg:tt)*) => {{
        log_with_timestamp(&format!($($arg)*));
    }};
}

macro_rules! debug {
    ($($arg:tt)*) => {{
        if DEBUG_LOGGING {
            log_with_timestamp(&format!($($arg)*));
        }
    }};
}

const SYS_UMCG_CTL: i64 = 450;
const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;

const WORKER_REGISTRATION_TIMEOUT_MS: u64 = 5000; // 5 second timeout for worker registration
const EVENT_BUFFER_SIZE: usize = 2;

#[derive(Debug, Copy, Clone)]
#[repr(u64)]
enum UmcgCmd {
    RegisterWorker = 1,
    RegisterServer,
    Unregister,
    Wake,
    Wait,
    CtxSwitch,
}

#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u64)]
enum UmcgEventType {
    Block = 1,
    Wake,
    Wait,
    Exit,
    Timeout,
    Preempt,
}

#[derive(Debug)]
pub enum ServerError {
    RegistrationFailed(i32),
    WorkerRegistrationTimeout { worker_id: usize },
    WorkerRegistrationFailed { worker_id: usize, error: i32 },
    InvalidWorkerEvent { worker_id: usize, event: u64 },
    SystemError(std::io::Error),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RegistrationFailed(err) => write!(f, "Server registration failed: {}", err),
            Self::WorkerRegistrationTimeout { worker_id } => {
                write!(f, "Worker {} registration timed out", worker_id)
            }
            Self::WorkerRegistrationFailed { worker_id, error } => {
                write!(f, "Worker {} registration failed: {}", worker_id, error)
            }
            Self::InvalidWorkerEvent { worker_id, event } => {
                write!(f, "Invalid event {} from worker {}", event, worker_id)
            }
            Self::SystemError(e) => write!(f, "System error: {}", e),
        }
    }
}

impl std::error::Error for ServerError {}

#[derive(Default)]
struct TaskStats {
    completed_tasks: Arc<Mutex<HashMap<Uuid, bool>>>,
    total_tasks: AtomicUsize,
    completed_count: AtomicUsize,
}

impl TaskStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            completed_tasks: Arc::new(Mutex::new(HashMap::new())),
            total_tasks: AtomicUsize::new(0),
            completed_count: AtomicUsize::new(0),
        })
    }

    fn register_task(&self, task_id: Uuid) {
        let mut tasks = self.completed_tasks.lock().unwrap();
        tasks.insert(task_id, false);
        self.total_tasks.fetch_add(1, Ordering::SeqCst);
        debug!("Registered task {}, total tasks: {}", task_id, self.total_tasks.load(Ordering::SeqCst));
    }

    fn mark_completed(&self, task_id: Uuid) {
        let mut tasks = self.completed_tasks.lock().unwrap();
        if let Some(completed) = tasks.get_mut(&task_id) {
            if !*completed {
                *completed = true;
                self.completed_count.fetch_add(1, Ordering::SeqCst);
                debug!("Completed task {}, total completed: {}/{}",
                    task_id,
                    self.completed_count.load(Ordering::SeqCst),
                    self.total_tasks.load(Ordering::SeqCst));
            }
        }
    }

    fn all_tasks_completed(&self) -> bool {
        let completed = self.completed_count.load(Ordering::SeqCst);
        let total = self.total_tasks.load(Ordering::SeqCst);
        completed == total && total > 0
    }
}

#[derive(Debug, Clone, PartialEq)]
enum WorkerStatus {
    Initializing,    // Worker thread started but not registered with UMCG
    Registering,     // UMCG registration in progress
    Running,         // Actively executing a task
    Blocked,         // Blocked on I/O or syscall
    Waiting,         // Ready for new tasks
    Completed,       // Worker has finished and unregistered
}

#[derive(Clone)]
struct ExecutorConfig {
    server_count: usize,
    worker_count: usize,
    start_cpu: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            server_count: 1,
            worker_count: 3,
            start_cpu: 0,
        }
    }
}

struct WorkerState {
    tid: pid_t,
    status: WorkerStatus,
    current_task: Option<Uuid>,
    server_id: usize,  // Track which server this worker belongs to
}

impl WorkerState {
    fn new(tid: pid_t, server_id: usize) -> Self {  // Add server_id parameter
        info!("Creating new WorkerState for tid {} on server {}", tid, server_id);
        Self {
            tid,
            status: WorkerStatus::Waiting,
            current_task: None,
            server_id,  // Set server_id
        }
    }
}

enum TaskState {
    Pending(Box<dyn FnOnce(&TaskHandle) + Send>),
    Running {
        worker_tid: i32,
        start_time: SystemTime,
        preempted: bool,
        blocked: bool,
        state: WorkerStatus,
    },
    Completed,
}

enum Task {
    Function(Box<dyn FnOnce(&TaskHandle) + Send>),
    Shutdown,
}

struct TaskEntry {
    id: Uuid,
    state: TaskState,
    priority: u32,
    enqueue_time: SystemTime,
}

impl TaskEntry {
    fn new(task: Box<dyn FnOnce(&TaskHandle) + Send>) -> Self {
        let id = Uuid::new_v4();
        info!("Creating new TaskEntry with ID: {}", id);
        Self {
            id,
            state: TaskState::Pending(task),
            priority: 0,
            enqueue_time: SystemTime::now(),
        }
    }

    fn is_resumable(&self) -> bool {
        matches!(&self.state,
            TaskState::Running { preempted: true, .. }
        )
    }
}

struct TaskQueue {
    pending: VecDeque<TaskEntry>,
    preempted: VecDeque<TaskEntry>,
    in_progress: HashMap<Uuid, TaskEntry>,
    mutex: Mutex<()>,
}

impl TaskQueue {
    fn new() -> Self {
        info!("Creating new TaskQueue");
        Self {
            pending: VecDeque::new(),
            preempted: VecDeque::new(),
            in_progress: HashMap::new(),
            mutex: Mutex::new(()),
        }
    }

    fn enqueue(&mut self, task: TaskEntry) {
        let _guard = self.mutex.lock().unwrap();
        info!("TaskQueue: Enqueueing task {}", task.id);
        match &task.state {
            TaskState::Pending(_) => {
                debug!("TaskQueue: Task {} added to pending queue", task.id);
                self.pending.push_back(task)
            },
            TaskState::Running { preempted: true, .. } => {
                debug!("TaskQueue: Task {} added to preempted queue", task.id);
                self.preempted.push_back(task)
            },
            TaskState::Running { .. } => {
                debug!("TaskQueue: Task {} added to in_progress map", task.id);
                self.in_progress.insert(task.id, task);
            }
            TaskState::Completed => {
                info!("Warning: Attempting to enqueue completed task {}", task.id);
            }
        }
        debug!("TaskQueue stats - Pending: {}, Preempted: {}, In Progress: {}",
               self.pending.len(), self.preempted.len(), self.in_progress.len());
    }

    fn get_next_task(&mut self) -> Option<TaskEntry> {
        let _guard = self.mutex.lock().unwrap();
        debug!("TaskQueue: Attempting to get next task");
        let task = self.preempted
            .pop_front()
            .or_else(|| self.pending.pop_front());

        if let Some(ref task) = task {
            debug!("TaskQueue: Retrieved task {}", task.id);
        } else {
            debug!("TaskQueue: No tasks available");
        }

        task
    }

    fn mark_in_progress(&mut self, task_id: Uuid, worker_tid: i32) {
        let _guard = self.mutex.lock().unwrap();
        debug!("TaskQueue: Marking task {} as in progress with worker {}", task_id, worker_tid);
        if let Some(task) = self.in_progress.get_mut(&task_id) {
            if let TaskState::Pending(_) = task.state {
                task.state = TaskState::Running {
                    worker_tid,
                    start_time: SystemTime::now(),
                    preempted: false,
                    blocked: false,
                    state: WorkerStatus::Running,
                };
                debug!("TaskQueue: Task {} state updated to Running", task_id);
            }
        }
    }
}

#[derive(Clone)]
struct TaskHandle {
    executor: Arc<Executor>,
}

impl TaskHandle {
    fn submit<F>(&self, f: F)
    where
        F: FnOnce(&TaskHandle) + Send + 'static,
    {
        info!("TaskHandle: Submitting new task");
        let task = Box::new(f);
        self.executor.submit(task);
        thread::sleep(Duration::from_millis(10));
        info!("TaskHandle: Task submitted successfully");
    }
}

#[derive(Clone)]
struct Worker {
    id: usize,
    tid: pid_t,
    handle: TaskHandle,
    tx: Sender<Task>,
    status: WorkerStatus,
    current_task: Option<Uuid>,
    server_id: usize,  // Keeping this from original worker state
}

impl Worker {
    fn new(id: usize, server_id: usize, tid: pid_t, handle: TaskHandle, tx: Sender<Task>) -> Self {
        info!("Creating new Worker {} for server {}", id, server_id);
        Self {
            id,
            tid,
            handle,
            tx,
            status: WorkerStatus::Initializing,
            current_task: None,
            server_id,
        }
    }

    fn assign_task(&self, mut task: TaskEntry, tx: &Sender<Task>) -> Result<(), TaskEntry> {
        info!("Worker {}: Starting task assignment", self.id);

        match &task.state {
            TaskState::Pending(_) => {
                if let TaskState::Pending(task_fn) = std::mem::replace(&mut task.state, TaskState::Completed) {
                    match tx.send(Task::Function(task_fn)) {
                        Ok(_) => {
                            debug!("Worker {}: Task sent successfully", self.id);
                            Ok(())
                        },
                        Err(_) => {
                            debug!("Worker {}: Channel is disconnected", self.id);
                            Err(task)
                        }
                    }
                } else {
                    unreachable!()
                }
            },
            _ => Ok(())
        }
    }
}

struct WorkerThread {
    id: usize,
    tid: pid_t,
    task_rx: Receiver<Task>,
    handle: TaskHandle,
    server_id: usize,
    cpu_id: usize,
}

impl WorkerThread {
    fn new(
        id: usize,
        server_id: usize,
        cpu_id: usize,
        executor: Arc<Executor>,
        task_rx: Receiver<Task>,
    ) -> Self {
        Self {
            id,
            tid: 0,
            task_rx,
            handle: TaskHandle { executor },
            server_id,
            cpu_id,
        }
    }
}

struct WorkerPool {
    workers: Arc<Mutex<HashMap<pid_t, Worker>>>,
    total_workers: usize,
}

impl WorkerPool {
    fn new(size: usize) -> Self {
        info!("Creating WorkerPool with capacity for {} workers", size);
        Self {
            workers: Arc::new(Mutex::new(HashMap::with_capacity(size))),
            total_workers: size,
        }
    }

    fn add_worker(&self, worker: Worker) {
        let mut workers = self.workers.lock().unwrap();
        debug!("WorkerPool: Adding worker {} with tid {}", worker.id, worker.tid);
        workers.insert(worker.tid, worker);
    }

    fn get_available_worker(&self) -> Option<(Worker, Sender<Task>)> {
        let workers = self.workers.lock().unwrap();
        let available = workers.iter()
            .find(|(_, w)| w.status == WorkerStatus::Waiting)
            .map(|(_, w)| {
                let worker = w.clone();
                let tx = w.tx.clone();
                (worker, tx)
            });
        debug!("WorkerPool: Found available worker: {:?}",
            available.as_ref().map(|(w, _)| w.id));
        available
    }

    fn update_worker_status(&self, tid: pid_t, status: WorkerStatus, task: Option<Uuid>) {
        if let Some(worker) = self.workers.lock().unwrap().get_mut(&tid) {
            debug!("WorkerPool: Updating worker {} status from {:?} to {:?}",
                worker.id, worker.status, status);
            worker.status = status;
            worker.current_task = task;
        }
    }

    fn update_worker_status_keep_task(&self, tid: pid_t, status: WorkerStatus) {
        if let Some(worker) = self.workers.lock().unwrap().get_mut(&tid) {
            debug!("WorkerPool: Updating worker {} status from {:?} to {:?} (keeping task {:?})",
                worker.id, worker.status, status, worker.current_task);
            worker.status = status;
            // Deliberately not touching current_task
        }
    }

    fn get_worker_state(&self, tid: pid_t) -> Option<(WorkerStatus, Option<Uuid>)> {
        self.workers.lock().unwrap()
            .get(&tid)
            .map(|w| (w.status.clone(), w.current_task))
    }
}

impl WorkerThread {
    fn start(mut self) -> (JoinHandle<()>, pid_t) {
        let (tid_tx, tid_rx) = channel();
        let id = self.id;
        info!("Starting WorkerThread {} for server {}", id, self.server_id);

        let handle = thread::spawn(move || {
            if let Err(e) = set_cpu_affinity(self.cpu_id) {
                debug!("Could not set CPU affinity for worker {} to CPU {}: {}",
                    id, self.cpu_id, e);
            }

            self.tid = get_thread_id();
            info!("Worker {}: Initialized with tid {}", self.id, self.tid);

            tid_tx.send(self.tid).expect("Failed to send worker tid");

            let worker_id = (self.tid as u64) << UMCG_WORKER_ID_SHIFT;
            debug!("Worker {}: Registering with UMCG (worker_id: {}) for server {}",
                self.id, worker_id, self.server_id);

            // Register with UMCG and specific server
            let reg_result = sys_umcg_ctl(
                0,
                UmcgCmd::RegisterWorker,
                0,
                worker_id,
                None,
                0
            );
            assert_eq!(reg_result, 0, "Worker {} UMCG registration failed", self.id);
            info!("Worker {}: UMCG registration complete with server {}",
                self.id, self.server_id);

            // // Wait immediately after registration
            // debug!("Worker {}: Entering initial wait state", self.id);
            // let wait_result = sys_umcg_ctl(
            //     0,
            //     UmcgCmd::Wait,
            //     0,
            //     0,
            //     None,
            //     0
            // );
            // assert_eq!(wait_result, 0, "Worker {} initial wait failed", self.id);

            debug!("Worker {}: Entering task processing loop", self.id);
            while let Ok(task) = self.task_rx.recv() {
                debug!("!!!!!!!!!! WORKER {}: Received task from channel !!!!!!!!!!!", self.id);
                match task {
                    Task::Function(task) => {
                        info!("!!!!!!!!!! WORKER {}: Starting task execution !!!!!!!!!!!", self.id);
                        task(&self.handle);
                        info!("!!!!!!!!!! WORKER {}: COMPLETED task execution !!!!!!!!!!!", self.id);

                        debug!("!!!!!!!!!! WORKER {} [{}]: Signaling ready for more work !!!!!!!!!!!",
                        self.id, self.tid);
                        let wait_result = sys_umcg_ctl(
                            self.server_id as u64,
                            UmcgCmd::Wait,
                            0,
                            0,
                            None,
                            0
                        );
                        debug!("!!!!!!!!!! WORKER {} [{}]: Wait syscall returned {} !!!!!!!!!!!",
                        self.id, self.tid, wait_result);
                        assert_eq!(wait_result, 0, "Worker {} UMCG wait failed", self.id);
                    }
                    Task::Shutdown => {
                        info!("Worker {}: Shutting down", self.id);
                        break;
                    }
                }
            }

            // Include server ID in unregister call
            info!("Worker {}: Beginning shutdown", self.id);
            let unreg_result = sys_umcg_ctl(
                self.server_id as u64,
                UmcgCmd::Unregister,
                0,
                0,
                None,
                0
            );
            assert_eq!(unreg_result, 0, "Worker {} UMCG unregistration failed", self.id);
            info!("Worker {}: Shutdown complete", self.id);
        });

        let tid = tid_rx.recv().expect("Failed to receive worker tid");
        info!("WorkerThread {} started with tid {} for server {}", id, tid, self.server_id);
        (handle, tid)
    }
}

#[derive(Clone)]
struct Server {
    id: usize,
    task_queue: Arc<Mutex<TaskQueue>>,
    worker_pool: Arc<WorkerPool>,
    executor: Arc<Executor>,
    completed_cycles: Arc<Mutex<HashMap<Uuid, bool>>>,
    done: Arc<AtomicBool>,
    cpu_id: usize,
}

impl Server {
    fn new(id: usize, cpu_id: usize, executor: Arc<Executor>) -> Self {
        log_with_timestamp(&format!("Creating Server {}", id));
        let worker_count = executor.config.worker_count;
        Self {
            id,
            task_queue: Arc::new(Mutex::new(TaskQueue::new())),
            worker_pool: Arc::new(WorkerPool::new(worker_count)),
            executor,
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            done: Arc::new(AtomicBool::new(false)),
            cpu_id
        }
    }

    fn initialize_workers(&self) -> Result<(), ServerError> {
        info!("Server {}: Initializing workers", self.id);

        for worker_id in 0..self.worker_pool.total_workers {
            info!("Server {}: Initializing worker {}", self.id, worker_id);

            let (tx, rx) = channel();
            let worker_thread = WorkerThread::new(
                worker_id,
                self.id,
                self.cpu_id,
                self.executor.clone(),
                rx,
            );

            // Start the worker thread
            let (_handle, tid) = worker_thread.start();

            // Create and add worker to pool
            let worker = Worker::new(
                worker_id,
                self.id,
                tid,
                TaskHandle { executor: self.executor.clone() },
                tx
            );

            self.worker_pool.add_worker(worker);
            self.worker_pool.update_worker_status(tid, WorkerStatus::Registering, None);

            // Wait for initial wake event
            let worker_event = self.wait_for_worker_registration(worker_id)?;
            debug!("Worker {}: Registering worker event {:?}", self.id, worker_event);

            self.worker_pool.update_worker_status(tid, WorkerStatus::Waiting, None);
            info!("Server {}: Worker {} initialized successfully", self.id, worker_id);
        }

        info!("Server {}: All workers initialized", self.id);
        Ok(())
    }

    fn wait_for_worker_registration(&self, worker_id: usize) -> Result<u64, ServerError> {
        let start = SystemTime::now();
        let timeout = Duration::from_millis(WORKER_REGISTRATION_TIMEOUT_MS);
        let mut events = [0u64; EVENT_BUFFER_SIZE];

        loop {
            if start.elapsed().unwrap() > timeout {
                return Err(ServerError::WorkerRegistrationTimeout { worker_id });
            }

            let ret = umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
            if ret != 0 {
                return Err(ServerError::WorkerRegistrationFailed {
                    worker_id,
                    error: ret
                });
            }

            let event = events[0];
            let event_type = event & UMCG_WORKER_EVENT_MASK;

            if event_type == UmcgEventType::Wake as u64 {
                return Ok(event);
            } else {
                debug!("Server {}: Unexpected event {} during worker {} registration",
                    self.id, event_type, worker_id);
            }
        }
    }

    pub fn add_task(&self, task: Task) {
        log_with_timestamp(&format!("Server {}: Adding new task", self.id));
        match task {
            Task::Function(f) => {
                let task_entry = TaskEntry::new(f);
                let mut queue = self.task_queue.lock().unwrap();
                queue.enqueue(task_entry);
                log_with_timestamp(&format!("Server {}: Task queued", self.id));
                // Remove the process_next_task call - let event loop handle it
            }
            Task::Shutdown => {
                log_with_timestamp(&format!("Server {}: Received shutdown signal", self.id));
                self.done.store(true, Ordering::Relaxed);
            }
        }
    }

    fn transition_worker_state(&self, worker_tid: pid_t, new_state: WorkerStatus, task_id: Option<Uuid>) {
        debug!("Server {}: Transitioning worker {} to {:?}",
        self.id, worker_tid, new_state);
        self.worker_pool.update_worker_status(worker_tid, new_state, task_id);
    }

    fn handle_umcg_event(&self, event: u64) -> Result<(), ServerError> {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        debug!("Server {}: Processing event {} for worker {}",
        self.id, event_type, worker_tid);

        match event_type {
            1 => { // BLOCK
                debug!("Server {}: Worker {} blocked", self.id, worker_tid);
                // Don't clear the task when blocking - keep current_task the same
                self.worker_pool.update_worker_status_keep_task(worker_tid, WorkerStatus::Blocked);
            },
            2 => { // WAKE
                /* IMPORTANT: Why we handle task completion in WAKE instead of WAIT
                 *
                 * The kernel/klib UMCG implementation has an interesting behavior:
                 * When a worker calls sys_umcg_ctl with UmcgCmd::Wait, internally this
                 * results in the server receiving a WAKE event (type 2) instead of a
                 * WAIT event (type 3).
                 *
                 * This happens because:
                 * 1. In the NanoVMs klib, when a worker calls Wait:
                 *    - The worker's status is set to UMCG_WORKER_WAITING
                 *    - The worker is added to a blockq
                 *    - The server is woken up via blockq_wake_one
                 *
                 * 2. This blockq wakeup mechanism results in a WAKE event being sent,
                 *    even though the worker's intention was to WAIT for more work.
                 *
                 * 3. Therefore, we need to differentiate between two types of WAKE events:
                 *    a) Wake after blocking (e.g., I/O, sleep) - worker needs to continue its current task
                 *    b) Wake after task completion - worker is ready for a new task
                 *
                 * We differentiate these cases by checking the worker's current state:
                 * - If worker was Running and we get a Wake: They completed a task
                 * - If worker was Blocked and we get a Wake: They can continue their task
                 * - If worker has no state: This is their initial registration wake
                 */
                debug!("!!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!");
                if let Some((current_status, current_task)) = self.worker_pool.get_worker_state(worker_tid) {
                    debug!("!!!!!!!!!! WAKE: Worker {} current status: {:?}, has task: {} !!!!!!!!!!", worker_tid, current_status, current_task.is_some());
                    match current_status {
                        WorkerStatus::Running => {
                            debug!("!!!!!!!!!! WAKE: This is a task completion WAKE (worker was Running) !!!!!!!!!!");
                            debug!("Server {}: Worker {} completing task", self.id, worker_tid);

                            // Mark worker as ready for new tasks
                            self.worker_pool.update_worker_status(worker_tid, WorkerStatus::Waiting, None);

                            if let Some(task_id) = current_task {
                                debug!("!!!!!!!!!! WAKE: Removing completed task {} and checking for more !!!!!!!!!!", task_id);
                                let mut task_queue = self.task_queue.lock().unwrap();
                                task_queue.in_progress.remove(&task_id);

                                // Immediately check for pending tasks
                                if let Some(next_task) = task_queue.get_next_task() {
                                    debug!("!!!!!!!!!! WAKE: Found pending task {} for worker {} !!!!!!!!!!",
                            next_task.id, worker_tid);

                                    // Update status before dropping task_queue lock
                                    self.worker_pool.update_worker_status(
                                        worker_tid,
                                        WorkerStatus::Running,
                                        Some(next_task.id)
                                    );

                                    task_queue.mark_in_progress(next_task.id, worker_tid);
                                    drop(task_queue);

                                    // Try to assign the task immediately
                                    if let Some((worker, tx)) = self.worker_pool.workers.lock().unwrap()
                                        .get(&worker_tid)
                                        .map(|w| (w.clone(), w.tx.clone()))
                                    {
                                        debug!("!!!!!!!!!! WAKE: Attempting to assign task to worker {} !!!!!!!!!!",
                                worker_tid);
                                        match worker.assign_task(next_task, &tx) {
                                            Ok(()) => {
                                                debug!("!!!!!!!!!! WAKE: Task assigned, doing context switch !!!!!!!!!!");
                                                if let Err(e) = self.context_switch_worker(worker_tid) {
                                                    error!("!!!!!!!!!! WAKE: Context switch failed: {} !!!!!!!!!!", e);
                                                    self.worker_pool.update_worker_status(
                                                        worker_tid,
                                                        WorkerStatus::Waiting,
                                                        None
                                                    );
                                                }
                                            }
                                            Err(failed_task) => {
                                                debug!("!!!!!!!!!! WAKE: Task assignment failed !!!!!!!!!!");
                                                let mut task_queue = self.task_queue.lock().unwrap();
                                                task_queue.enqueue(failed_task);
                                                self.worker_pool.update_worker_status(
                                                    worker_tid,
                                                    WorkerStatus::Waiting,
                                                    None
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    debug!("!!!!!!!!!! WAKE: No pending tasks found !!!!!!!!!!");
                                }
                            }
                        },
                        WorkerStatus::Blocked => {
                            debug!("!!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!");
                            debug!("Server {}: Worker {} unblocking", self.id, worker_tid);
                            // Keep the current task - worker is resuming after being blocked
                            self.worker_pool.update_worker_status(
                                worker_tid,
                                WorkerStatus::Running,
                                current_task
                            );

                            debug!("Server {}: switching back to worker after sleep/io WAKE. Worker {} ", self.id, worker_tid);
                            // Context switch back to the worker to let it continue its task
                            if let Err(e) = self.context_switch_worker(worker_tid) {
                                error!("Failed to context switch back to unblocked worker {}: {}", worker_tid, e);
                            }
                        },
                        WorkerStatus::Waiting => {
                            debug!("!!!!!!!!!! WAKE: This is a Wait->Wake from klib (worker already Waiting) !!!!!!!!!!");
                            let mut task_queue = self.task_queue.lock().unwrap();
                            if let Some(task) = task_queue.get_next_task() {
                                debug!("!!!!!!!!!! WAKE: Found task for waiting worker !!!!!!!!!!");
                                self.worker_pool.update_worker_status(
                                    worker_tid,
                                    WorkerStatus::Running,
                                    Some(task.id)
                                );
                                task_queue.mark_in_progress(task.id, worker_tid);
                                drop(task_queue);

                                if let Some((worker, tx)) = self.worker_pool.workers.lock().unwrap()
                                    .get(&worker_tid)
                                    .map(|w| (w.clone(), w.tx.clone()))
                                {
                                    match worker.assign_task(task, &tx) {
                                        Ok(()) => {
                                            debug!("!!!!!!!!!! WAKE: Assigned task to waiting worker !!!!!!!!!!");
                                            if let Err(e) = self.context_switch_worker(worker_tid) {
                                                error!("!!!!!!!!!! WAKE: Context switch failed for waiting worker !!!!!!!!!!");
                                                self.worker_pool.update_worker_status(
                                                    worker_tid,
                                                    WorkerStatus::Waiting,
                                                    None
                                                );
                                            }
                                        }
                                        Err(failed_task) => {
                                            debug!("!!!!!!!!!! WAKE: Task assignment failed for waiting worker !!!!!!!!!!");
                                            let mut task_queue = self.task_queue.lock().unwrap();
                                            task_queue.enqueue(failed_task);
                                            self.worker_pool.update_worker_status(
                                                worker_tid,
                                                WorkerStatus::Waiting,
                                                None
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            debug!("!!!!!!!!!! WAKE: This is likely an initial registration WAKE (worker status: {:?}) !!!!!!!!!!",
                    current_status);
                            debug!("Server {}: Worker {} in initial state", self.id, worker_tid);
                            self.worker_pool.update_worker_status(
                                worker_tid,
                                WorkerStatus::Waiting,
                                None
                            );
                        }
                    }
                } else {
                    debug!("!!! NO FUCKING WORKER STATE FOR WORKER {}!!", worker_tid);
                }
            },
            3 => { // WAIT
                debug!("!!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!");
                debug!("Server {}: Got explicit WAIT from worker {}", self.id, worker_tid);
                if let Some((current_status, _)) = self.worker_pool.get_worker_state(worker_tid) {
                    debug!("Server {}: Worker {} current status: {:?}", self.id, worker_tid, current_status);
                    if current_status != WorkerStatus::Waiting {
                        self.worker_pool.update_worker_status(worker_tid, WorkerStatus::Waiting, None);
                    }
                }
            },
            4 => { // EXIT
                debug!("Server {}: Worker {} exited", self.id, worker_tid);
                self.worker_pool.update_worker_status(worker_tid, WorkerStatus::Completed, None);
            },
            _ => {
                return Err(ServerError::InvalidWorkerEvent {
                    worker_id: worker_tid as usize,
                    event
                });
            }
        }
        Ok(())
    }

    fn start(self) -> JoinHandle<()> {
        thread::spawn(move || {
            if let Err(e) = self.run_server() {
                error!("Server {} failed: {}", self.id, e);
            }
        })
    }

    fn run_server(&self) -> Result<(), ServerError> {
        if let Err(e) = set_cpu_affinity(self.cpu_id) {
            return Err(ServerError::SystemError(e));
        }

        info!("Server {}: Starting up", self.id);

        let reg_result = sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0);
        if reg_result != 0 {
            return Err(ServerError::RegistrationFailed(reg_result));
        }
        info!("Server {}: UMCG registration complete", self.id);

        // Initialize workers
        self.initialize_workers()?;

        // Main UMCG event loop - now handles both events and task management
        // Main UMCG event loop - now handles both events and task management
        while !self.done.load(Ordering::Relaxed) {
            // Try to schedule any pending tasks first
            debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!");
            self.try_schedule_tasks()?;

            let mut events = [0u64; 6];
            debug!("!!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!");
            // Add a short timeout (e.g., 100ms) so we don't block forever
            let ret = sys_umcg_ctl(
                0,
                UmcgCmd::Wait,
                0,
                100_000_000, // 100ms in nanoseconds
                Some(&mut events),
                6
            );
            debug!("!!!!!!!!!! SERVER EVENT WAIT RETURNED: {} !!!!!!!!!!", ret);

            if ret != 0 && unsafe { *libc::__errno_location() } != libc::ETIMEDOUT {
                error!("Server {} wait error: {}", self.id, ret);
                return Err(ServerError::SystemError(std::io::Error::last_os_error()));
            }

            for &event in events.iter().take_while(|&&e| e != 0) {
                debug!("!!!!!!!!!! SERVER PROCESSING EVENT: {} !!!!!!!!!!", event);
                // Use our existing event handler
                if let Err(e) = self.handle_umcg_event(event) {
                    error!("Server {} event handling error: {}", self.id, e);
                }

                // Try to schedule tasks after handling each event too
                self.try_schedule_tasks()?;
            }
        }

        self.initiate_shutdown();
        info!("Server {}: Shutdown complete", self.id);
        Ok(())
    }

    fn try_schedule_tasks(&self) -> Result<(), ServerError> {
        debug!("Server {}: Attempting to schedule tasks...", self.id);
        let mut workers = self.worker_pool.workers.lock().unwrap();
        debug!("Server {}: Looking for waiting workers among {} workers",
        self.id, workers.len());
        if let Some((_, worker)) = workers.iter_mut().find(|(_, w)| w.status == WorkerStatus::Waiting) {
            debug!("Server {}: Found waiting worker {} with status {:?}",
            self.id, worker.id, worker.status);
            let tx = worker.tx.clone();
            let worker_tid = worker.tid;
            let worker_clone = worker.clone();
            drop(workers);

            let mut task_queue = self.task_queue.lock().unwrap();
            if let Some(task) = task_queue.get_next_task() {
                info!("Server {}: Assigning task to worker {}", self.id, worker_clone.id);

                self.worker_pool.update_worker_status(
                    worker_tid,
                    WorkerStatus::Running,
                    Some(task.id)
                );

                task_queue.mark_in_progress(task.id, worker_tid);
                drop(task_queue);

                match worker_clone.assign_task(task, &tx) {
                    Ok(()) => {
                        match self.context_switch_worker(worker_tid) {
                            Ok(()) => {
                                debug!("Server {}: Successfully switched to worker {}",
                                self.id, worker_tid);
                            }
                            Err(e) => {
                                debug!("Server {}: Context switch failed for worker {}: {}",
                                self.id, worker_tid, e);
                                self.worker_pool.update_worker_status(
                                    worker_tid,
                                    WorkerStatus::Waiting,
                                    None
                                );
                                return Err(e);
                            }
                        }
                    }
                    Err(failed_task) => {
                        let mut task_queue = self.task_queue.lock().unwrap();
                        task_queue.enqueue(failed_task);
                        self.worker_pool.update_worker_status(
                            worker_tid,
                            WorkerStatus::Waiting,
                            None
                        );
                    }
                }
            }
        } else {
            debug!("Server {}: No waiting workers found", self.id);
        }
        Ok(())
    }

    fn initiate_shutdown(&self) {
        info!("Server {}: Beginning shutdown sequence", self.id);
        self.done.store(true, Ordering::SeqCst);

        // Signal all workers to shutdown
        let workers = self.worker_pool.workers.lock().unwrap();
        for (worker_tid, worker) in workers.iter() {
            debug!("Server {}: Sending shutdown signal to worker {}", self.id, worker.id);
            if let Err(e) = worker.tx.send(Task::Shutdown) {
                error!("Server {}: Failed to send shutdown to worker {}: {}",
                self.id, worker.id, e);
            }
        }

        // Final UMCG cleanup
        info!("Server {}: Unregistering from UMCG", self.id);
        let unreg_result = sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
        if unreg_result != 0 {
            error!("Server {}: UMCG unregister failed: {}", self.id, unreg_result);
        }
    }

    fn context_switch_worker(&self, worker_tid: pid_t) -> Result<(), ServerError> {
        let mut events = [0u64; 2];
        debug!("Server {}: Context switching to worker {}", self.id, worker_tid);

        let switch_result = sys_umcg_ctl(
            0,
            UmcgCmd::CtxSwitch,
            worker_tid,
            0,
            Some(&mut events),
            2
        );

        if switch_result != 0 {
            return Err(ServerError::SystemError(std::io::Error::last_os_error()));
        }

        // Process any events from the context switch
        for &event in events.iter().take_while(|&&e| e != 0) {
            self.handle_umcg_event(event)?;
        }

        Ok(())
    }
}

struct Executor {
    servers: Mutex<Vec<Server>>,
    next_server: AtomicUsize,
    config: ExecutorConfig,
    task_stats: Arc<TaskStats>,  // Add task stats
}

impl Executor {
    fn new(config: ExecutorConfig) -> Arc<Self> {
        log_with_timestamp("Creating new Executor");
        let executor = Arc::new(Self {
            servers: Mutex::new(Vec::with_capacity(config.server_count)),
            next_server: AtomicUsize::new(0),
            config: config.clone(),
            task_stats: TaskStats::new(),
        });

        let executor_clone = executor.clone();
        {
            let mut servers = executor_clone.servers.lock().unwrap();

            for i in 0..config.server_count {
                let cpu_id = config.start_cpu + i;
                log_with_timestamp(&format!("Creating Server {} on CPU {}", i, cpu_id));
                servers.push(Server::new(i, cpu_id, executor_clone.clone()));
            }
        }

        executor
    }

    fn submit<F>(&self, f: F)
    where
        F: FnOnce(&TaskHandle) + Send + 'static,
    {
        info!("Executor: Submitting new task");
        let task_id = Uuid::new_v4();
        self.task_stats.register_task(task_id);

        let stats = self.task_stats.clone();
        let wrapped_task = Box::new(move |handle: &TaskHandle| {
            f(handle);
            stats.mark_completed(task_id);
        });

        let server_idx = self.next_server.fetch_add(1, Ordering::Relaxed) % self.config.server_count;
        let servers = self.servers.lock().unwrap();

        if let Some(server) = servers.get(server_idx) {
            let worker_count = server.worker_pool.workers.lock().unwrap().len();
            info!("Executor: Adding task {} to server {} (has {} workers)",
                task_id, server_idx, worker_count);
            server.add_task(Task::Function(wrapped_task));
            debug!("Task {} assigned to server {}", task_id, server_idx);
        }
    }

    fn start(&self) {
        info!("Executor: Starting servers");
        let servers = self.servers.lock().unwrap();
        for server in servers.iter() {
            server.clone().start();
        }
        info!("Executor: All servers started");
    }

    pub fn all_tasks_completed(&self) -> bool {
        self.task_stats.all_tasks_completed()
    }

    // Add method to get completion stats
    pub fn get_completion_stats(&self) -> (usize, usize) {
        (
            self.task_stats.completed_count.load(Ordering::SeqCst),
            self.task_stats.total_tasks.load(Ordering::SeqCst)
        )
    }

    pub fn shutdown(&self) {
        log_with_timestamp("Initiating executor shutdown...");
        let servers = self.servers.lock().unwrap();
        for server in servers.iter() {
            server.add_task(Task::Shutdown);
        }
    }
}

fn get_thread_id() -> pid_t {
    unsafe { syscall(SYS_gettid) as pid_t }
}

fn set_cpu_affinity(cpu_id: usize) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        let set_ref = &mut *set.as_mut_ptr();

        libc::CPU_ZERO(set_ref);
        libc::CPU_SET(cpu_id, set_ref);

        let result = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set.as_ptr());
        if result != 0 {
            debug!("Could not set CPU affinity to {}: {}", cpu_id, std::io::Error::last_os_error());
        } else {
            debug!("Successfully set CPU affinity to {}", cpu_id);
        }
    }
    Ok(())
}

fn sys_umcg_ctl(
    flags: u64,
    cmd: UmcgCmd,
    next_tid: pid_t,
    abs_timeout: u64,
    events: Option<&mut [u64]>,
    event_sz: i32,
) -> i32 {
    debug!("UMCG syscall - cmd: {:?}, tid: {}, flags: {}", cmd, next_tid, flags);
    let result = unsafe {
        syscall(
            SYS_UMCG_CTL,
            flags as i64,
            cmd as i64,
            next_tid as i64,
            abs_timeout as i64,
            events.map_or(std::ptr::null_mut(), |e| e.as_mut_ptr()) as i64,
            event_sz as i64,
        ) as i32
    };
    debug!("UMCG syscall result: {}", result);
    result
}

fn umcg_wait_retry(worker_id: u64, mut events_buf: Option<&mut [u64]>, event_sz: i32) -> i32 {
    let mut flags = 0;
    loop {
        debug!("!!!!!!!!!! UMCG WAIT RETRY START - worker: {}, flags: {} !!!!!!!!!!", worker_id, flags);
        let events = events_buf.as_deref_mut();
        let ret = sys_umcg_ctl(
            flags,
            UmcgCmd::Wait,
            (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            events,
            event_sz,
        );
        debug!("!!!!!!!!!! UMCG WAIT RETRY RETURNED: {} !!!!!!!!!!", ret);
        if ret != -1 || unsafe { *libc::__errno_location() } != EINTR {
            return ret;
        }
        flags = UMCG_WAIT_FLAG_INTERRUPTED;
    }
}

fn log_with_timestamp(msg: &str) {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    println!("[{:>3}.{:03}] {}",
             now.as_secs() % 1000,
             now.subsec_millis(),
             msg);
}



// Demo functions remain unchanged
pub fn run_dynamic_task_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 1,
        worker_count: 3,
        start_cpu: 0,
    };
    let executor = Executor::new(config);

    log_with_timestamp("Starting executor...");
    executor.start();

    thread::sleep(Duration::from_millis(50));

    log_with_timestamp("Submitting initial tasks...");

    for i in 0..6 {
        let task = move |handle: &TaskHandle| {
            log_with_timestamp(&format!("!!!! Initial task {}: STARTING task !!!!", i));

            log_with_timestamp(&format!("!!!! Initial task {}: ABOUT TO SLEEP !!!!", i));
            thread::sleep(Duration::from_secs(2));
            log_with_timestamp(&format!("!!!! Initial task {}: WOKE UP FROM SLEEP !!!!", i));

            log_with_timestamp(&format!("!!!! Initial task {}: PREPARING to spawn child task !!!!", i));

            let parent_id = i;
            handle.submit(move |_| {
                log_with_timestamp(&format!("!!!! Child task of initial task {}: STARTING work !!!!", parent_id));
                thread::sleep(Duration::from_secs(1));
                log_with_timestamp(&format!("!!!! Child task of initial task {}: COMPLETED !!!!", parent_id));
            });

            log_with_timestamp(&format!("!!!! Initial task {}: COMPLETED !!!!", i));
        };

        log_with_timestamp(&format!("!!!! Submitting initial task {} !!!!", i));
        executor.submit(Box::new(task));
    }

    log_with_timestamp("All tasks submitted, waiting for completion...");

    // Wait for completion with timeout and progress updates
    let start = Instant::now();
    let timeout = Duration::from_secs(30);

    while !executor.all_tasks_completed() {
        if start.elapsed() > timeout {
            let (completed, total) = executor.get_completion_stats();
            log_with_timestamp(&format!(
                "Timeout waiting for tasks to complete! ({}/{} completed)",
                completed, total
            ));
            executor.shutdown();
            return 1;
        }

        if start.elapsed().as_secs() % 5 == 0 {
            let (completed, total) = executor.get_completion_stats();
            log_with_timestamp(&format!("Progress: {}/{} tasks completed", completed, total));
        }

        thread::sleep(Duration::from_millis(100));
    }

    let (completed, total) = executor.get_completion_stats();
    log_with_timestamp(&format!("All tasks completed successfully ({}/{})", completed, total));

    // Clean shutdown
    executor.shutdown();
    0
}

pub fn run_multi_server_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 3,
        worker_count: 3,
        start_cpu: 0,
    };
    let executor = Executor::new(config);

    log_with_timestamp("Starting executor...");
    executor.start();

    thread::sleep(Duration::from_millis(100));

    log_with_timestamp("Submitting tasks to multiple servers...");

    for i in 0..9 {
        let task = move |handle: &TaskHandle| {
            log_with_timestamp(&format!("Task {}: Starting execution", i));
            thread::sleep(Duration::from_secs(1));

            let parent_id = i;
            handle.submit(move |_| {
                log_with_timestamp(&format!("Child task of initial task {}: Starting work", parent_id));
                thread::sleep(Duration::from_millis(500));
                log_with_timestamp(&format!("Child task of initial task {}: Completed", parent_id));
            });

            log_with_timestamp(&format!("Task {}: Completed", i));
        };

        executor.submit(Box::new(task));
    }

    log_with_timestamp("All tasks submitted, waiting for completion...");
    thread::sleep(Duration::from_secs(15));
    0
}

/*
➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[160.466] Creating new Executor
[160.481] Creating Server 0 on CPU 0
[160.488] Creating Server 0
[160.490] Creating new TaskQueue
[160.493] Creating WorkerPool with capacity for 3 workers
[160.496] Starting executor...
[160.496] Executor: Starting servers
[160.516] Executor: All servers started
[160.529] Successfully set CPU affinity to 0
[160.539] Server 0: Starting up
[160.541] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[160.545] UMCG syscall result: 0
[160.546] Server 0: UMCG registration complete
[160.546] Server 0: Initializing workers
[160.547] Server 0: Initializing worker 0
[160.549] Starting WorkerThread 0 for server 0
[160.559] Successfully set CPU affinity to 0
[160.560] Worker 0: Initialized with tid 4
[160.561] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[160.562] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[160.568] WorkerThread 0 started with tid 4 for server 0
[160.569] Creating new Worker 0 for server 0
[160.569] WorkerPool: Adding worker 0 with tid 4
[160.572] WorkerPool: Updating worker 0 status from Initializing to Registering
[160.573] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[160.578] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[160.580] UMCG syscall result: 0
[160.581] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[160.581] Submitting initial tasks...
[160.582] !!!! Submitting initial task 0 !!!!
[160.582] Worker 0: Registering worker event 130
[160.583] Executor: Submitting new task
[160.583] WorkerPool: Updating worker 0 status from Registering to Waiting
[160.584] Server 0: Worker 0 initialized successfully
[160.585] Server 0: Initializing worker 1
[160.598] Registered task 3994e194-4d6e-4be9-9d41-0a147f8b78dd, total tasks: 1
[160.601] Starting WorkerThread 1 for server 0
[160.603] Executor: Adding task 3994e194-4d6e-4be9-9d41-0a147f8b78dd to server 0 (has 1 workers)
[160.605] Server 0: Adding new task
[160.612] Successfully set CPU affinity to 0
[160.613] Worker 1: Initialized with tid 5
[160.608] Creating new TaskEntry with ID: f904fd90-5250-4b10-9d16-f3688543dede
[160.619] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[160.620] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[160.620] WorkerThread 1 started with tid 5 for server 0
[160.618] TaskQueue: Enqueueing task f904fd90-5250-4b10-9d16-f3688543dede
[160.622] Creating new Worker 1 for server 0
[160.622] TaskQueue: Task f904fd90-5250-4b10-9d16-f3688543dede added to pending queue
[160.624] WorkerPool: Adding worker 1 with tid 5
[160.625] WorkerPool: Updating worker 1 status from Initializing to Registering
[160.624] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[160.625] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[160.626] Server 0: Task queued
[160.628] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[160.629] UMCG syscall result: 0
[160.629] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[160.629] Worker 0: Registering worker event 162
[160.630] WorkerPool: Updating worker 1 status from Registering to Waiting
[160.630] Server 0: Worker 1 initialized successfully
[160.628] Task 3994e194-4d6e-4be9-9d41-0a147f8b78dd assigned to server 0
[160.630] Server 0: Initializing worker 2
[160.631] !!!! Submitting initial task 1 !!!!
[160.631] Starting WorkerThread 2 for server 0
[160.632] Executor: Submitting new task
[160.634] Successfully set CPU affinity to 0
[160.634] Worker 2: Initialized with tid 6
[160.633] Registered task 477ca39d-3b4b-4cad-97b7-ffc56db86a54, total tasks: 2
[160.635] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[160.636] WorkerThread 2 started with tid 6 for server 0
[160.636] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[160.636] Executor: Adding task 477ca39d-3b4b-4cad-97b7-ffc56db86a54 to server 0 (has 2 workers)
[160.642] Server 0: Adding new task
[160.643] Creating new TaskEntry with ID: a7c225c2-df52-46f0-a9fd-8c9f02049a59
[160.644] TaskQueue: Enqueueing task a7c225c2-df52-46f0-a9fd-8c9f02049a59
[160.644] Creating new Worker 2 for server 0
[160.645] TaskQueue: Task a7c225c2-df52-46f0-a9fd-8c9f02049a59 added to pending queue
[160.645] WorkerPool: Adding worker 2 with tid 6
[160.646] WorkerPool: Updating worker 2 status from Initializing to Registering
[160.646] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[160.646] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[160.647] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[160.649] Server 0: Task queued
[160.650] UMCG syscall result: 0
[160.650] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[160.650] Worker 0: Registering worker event 194
[160.650] Task 477ca39d-3b4b-4cad-97b7-ffc56db86a54 assigned to server 0
[160.651] WorkerPool: Updating worker 2 status from Registering to Waiting
[160.651] !!!! Submitting initial task 2 !!!!
[160.651] Executor: Submitting new task
[160.652] Server 0: Worker 2 initialized successfully
[160.652] Registered task d26821db-c873-46f5-80e4-e71ecd35a4f7, total tasks: 3
[160.652] Executor: Adding task d26821db-c873-46f5-80e4-e71ecd35a4f7 to server 0 (has 3 workers)
[160.652] Server 0: All workers initialized
[160.653] Server 0: Adding new task
[160.653] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[160.654] Creating new TaskEntry with ID: cc7c0b99-e3da-4f3b-86c4-8bac54d55acc
[160.654] Server 0: Attempting to schedule tasks...
[160.655] TaskQueue: Enqueueing task cc7c0b99-e3da-4f3b-86c4-8bac54d55acc
[160.656] Server 0: Looking for waiting workers among 3 workers
[160.656] TaskQueue: Task cc7c0b99-e3da-4f3b-86c4-8bac54d55acc added to pending queue
[160.659] Server 0: Found waiting worker 1 with status Waiting
[160.659] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[160.660] Server 0: Task queued
[160.661] Task d26821db-c873-46f5-80e4-e71ecd35a4f7 assigned to server 0
[160.661] !!!! Submitting initial task 3 !!!!
[160.661] Executor: Submitting new task
[160.662] TaskQueue: Attempting to get next task
[160.663] TaskQueue: Retrieved task f904fd90-5250-4b10-9d16-f3688543dede
[160.663] Registered task 22483a39-592c-4576-8661-d751cf3dcdcb, total tasks: 4
[160.664] Executor: Adding task 22483a39-592c-4576-8661-d751cf3dcdcb to server 0 (has 3 workers)
[160.665] Server 0: Adding new task
[160.664] Server 0: Assigning task to worker 1
[160.666] Creating new TaskEntry with ID: ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa
[160.667] WorkerPool: Updating worker 1 status from Waiting to Running
[160.668] TaskQueue: Marking task f904fd90-5250-4b10-9d16-f3688543dede as in progress with worker 5
[160.669] TaskQueue: Enqueueing task ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa
[160.669] TaskQueue: Task ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa added to pending queue
[160.669] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[160.670] Server 0: Task queued
[160.670] Worker 1: Starting task assignment
[160.670] Task 22483a39-592c-4576-8661-d751cf3dcdcb assigned to server 0
[160.672] !!!! Submitting initial task 4 !!!!
[160.672] Executor: Submitting new task
[160.671] Worker 1: Task sent successfully
[160.673] Registered task 43707f4d-2013-4b0b-bcb7-812cf4f38474, total tasks: 5
[160.673] Server 0: Context switching to worker 5
[160.675] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[160.674] Executor: Adding task 43707f4d-2013-4b0b-bcb7-812cf4f38474 to server 0 (has 3 workers)
[160.680] Server 0: Adding new task
[160.679] UMCG syscall result: 0
[160.680] Creating new TaskEntry with ID: 34be1d03-48ac-4616-b80a-d683c6aa6b17
[160.681] TaskQueue: Enqueueing task 34be1d03-48ac-4616-b80a-d683c6aa6b17
[160.682] TaskQueue: Task 34be1d03-48ac-4616-b80a-d683c6aa6b17 added to pending queue
[160.681] Worker 1: UMCG registration complete with server 0
[160.684] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[160.684] UMCG syscall result: 0
[160.685] Server 0: Processing event 1 for worker 5
[160.685] Server 0: Task queued
[160.686] Task 43707f4d-2013-4b0b-bcb7-812cf4f38474 assigned to server 0
[160.686] Server 0: Worker 5 blocked
[160.687] !!!! Submitting initial task 5 !!!!
[160.688] Executor: Submitting new task
[160.688] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(f904fd90-5250-4b10-9d16-f3688543dede))
[160.691] Registered task 20d86a84-3a93-4a2f-a079-788aa3fb3fac, total tasks: 6
[160.691] Server 0: Successfully switched to worker 5
[160.692] Executor: Adding task 20d86a84-3a93-4a2f-a079-788aa3fb3fac to server 0 (has 3 workers)
[160.692] Server 0: Adding new task
[160.693] Creating new TaskEntry with ID: c6d9ffbc-b277-4312-b82d-a6c41108343a
[160.695] TaskQueue: Enqueueing task c6d9ffbc-b277-4312-b82d-a6c41108343a
[160.695] TaskQueue: Task c6d9ffbc-b277-4312-b82d-a6c41108343a added to pending queue
[160.695] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[160.696] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[160.697] UMCG syscall result: 0
[160.698] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[160.698] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[160.698] Server 0: Task queued
[160.699] Task 20d86a84-3a93-4a2f-a079-788aa3fb3fac assigned to server 0
[160.699] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[160.699] All tasks submitted, waiting for completion...
[160.700] Server 0: Processing event 2 for worker 5
[160.701] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[160.703] Progress: 0/6 tasks completed
[160.704] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[160.705] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[160.705] Server 0: Worker 5 unblocking
[160.706] WorkerPool: Updating worker 1 status from Blocked to Running
[160.706] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[160.707] Server 0: Context switching to worker 5
[160.707] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[160.709] Worker 1: Entering task processing loop
[160.710] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[160.710] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[160.711] !!!! Initial task 0: STARTING task !!!!
[160.711] !!!! Initial task 0: ABOUT TO SLEEP !!!!
[160.712] UMCG syscall result: 0
[160.712] Server 0: Processing event 1 for worker 5
[160.713] Server 0: Worker 5 blocked
[160.714] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(f904fd90-5250-4b10-9d16-f3688543dede))
[160.714] Server 0: Attempting to schedule tasks...
[160.715] Server 0: Looking for waiting workers among 3 workers
[160.715] Server 0: Found waiting worker 0 with status Waiting
[160.716] TaskQueue: Attempting to get next task
[160.716] TaskQueue: Retrieved task a7c225c2-df52-46f0-a9fd-8c9f02049a59
[160.717] Server 0: Assigning task to worker 0
[160.717] WorkerPool: Updating worker 0 status from Waiting to Running
[160.717] TaskQueue: Marking task a7c225c2-df52-46f0-a9fd-8c9f02049a59 as in progress with worker 4
[160.718] Worker 0: Starting task assignment
[160.718] Worker 0: Task sent successfully
[160.719] Server 0: Context switching to worker 4
[160.719] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[160.719] UMCG syscall result: 0
[160.720] Worker 0: UMCG registration complete with server 0
[160.720] Worker 0: Entering task processing loop
[160.721] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[160.721] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[160.721] !!!! Initial task 1: STARTING task !!!!
[160.722] !!!! Initial task 1: ABOUT TO SLEEP !!!!
[160.722] UMCG syscall result: 0
[160.722] Server 0: Processing event 1 for worker 4
[160.722] Server 0: Worker 4 blocked
[160.723] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(a7c225c2-df52-46f0-a9fd-8c9f02049a59))
[160.723] Server 0: Successfully switched to worker 4
[160.724] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[160.724] Server 0: Attempting to schedule tasks...
[160.724] Server 0: Looking for waiting workers among 3 workers
[160.724] Server 0: Found waiting worker 2 with status Waiting
[160.725] TaskQueue: Attempting to get next task
[160.725] TaskQueue: Retrieved task cc7c0b99-e3da-4f3b-86c4-8bac54d55acc
[160.725] Server 0: Assigning task to worker 2
[160.725] WorkerPool: Updating worker 2 status from Waiting to Running
[160.726] TaskQueue: Marking task cc7c0b99-e3da-4f3b-86c4-8bac54d55acc as in progress with worker 6
[160.726] Worker 2: Starting task assignment
[160.726] Worker 2: Task sent successfully
[160.727] Server 0: Context switching to worker 6
[160.727] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[160.728] UMCG syscall result: 0
[160.728] Worker 2: UMCG registration complete with server 0
[160.728] Worker 2: Entering task processing loop
[160.729] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[160.729] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[160.729] !!!! Initial task 2: STARTING task !!!!
[160.730] !!!! Initial task 2: ABOUT TO SLEEP !!!!
[160.730] UMCG syscall result: 0
[160.730] Server 0: Processing event 1 for worker 6
[160.730] Server 0: Worker 6 blocked
[160.731] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(cc7c0b99-e3da-4f3b-86c4-8bac54d55acc))
[160.732] Server 0: Successfully switched to worker 6
[160.732] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[160.732] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[160.813] Progress: 0/6 tasks completed
[160.919] Progress: 0/6 tasks completed
[161.058] Progress: 0/6 tasks completed
[161.169] Progress: 0/6 tasks completed
[161.274] Progress: 0/6 tasks completed
[161.380] Progress: 0/6 tasks completed
[161.487] Progress: 0/6 tasks completed
[161.595] Progress: 0/6 tasks completed
en1: assigned FE80::D08A:1FFF:FE40:4512
[162.730] UMCG syscall result: 0
[162.731] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[162.733] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[162.734] Server 0: Processing event 2 for worker 5
[162.734] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.736] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[162.737] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.737] Server 0: Worker 5 unblocking
[162.738] WorkerPool: Updating worker 1 status from Blocked to Running
[162.739] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[162.739] Server 0: Context switching to worker 5
[162.739] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[162.739] !!!! Initial task 0: WOKE UP FROM SLEEP !!!!
[162.740] !!!! Initial task 0: PREPARING to spawn child task !!!!
[162.740] TaskHandle: Submitting new task
[162.741] Executor: Submitting new task
[162.742] Registered task 02d11542-801c-4c9e-9b62-bb5527d83dff, total tasks: 7
[162.744] Executor: Adding task 02d11542-801c-4c9e-9b62-bb5527d83dff to server 0 (has 3 workers)
[162.745] Server 0: Adding new task
[162.746] Creating new TaskEntry with ID: b7323d59-f3ce-4daf-a607-215c10ed08ae
[162.746] TaskQueue: Enqueueing task b7323d59-f3ce-4daf-a607-215c10ed08ae
[162.747] TaskQueue: Task b7323d59-f3ce-4daf-a607-215c10ed08ae added to pending queue
[162.747] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[162.748] Server 0: Task queued
[162.749] Task 02d11542-801c-4c9e-9b62-bb5527d83dff assigned to server 0
[162.750] UMCG syscall result: 0
[162.750] Server 0: Processing event 1 for worker 5
[162.750] Server 0: Worker 5 blocked
[162.751] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(f904fd90-5250-4b10-9d16-f3688543dede))
[162.751] Server 0: Attempting to schedule tasks...
[162.751] Server 0: Looking for waiting workers among 3 workers
[162.752] Server 0: No waiting workers found
[162.753] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[162.753] Server 0: Attempting to schedule tasks...
[162.753] Server 0: Looking for waiting workers among 3 workers
[162.754] Server 0: No waiting workers found
[162.755] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[162.755] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.756] UMCG syscall result: 0
[162.757] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[162.757] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[162.757] Server 0: Processing event 2 for worker 4
[162.758] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.758] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[162.759] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.759] Server 0: Worker 4 unblocking
[162.759] WorkerPool: Updating worker 0 status from Blocked to Running
[162.760] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[162.760] Server 0: Context switching to worker 4
[162.761] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[162.761] !!!! Initial task 1: WOKE UP FROM SLEEP !!!!
[162.762] !!!! Initial task 1: PREPARING to spawn child task !!!!
[162.762] TaskHandle: Submitting new task
[162.762] Executor: Submitting new task
[162.764] Registered task 5c342960-dbd4-4803-8cf0-7fc1876f08f5, total tasks: 8
[162.765] Executor: Adding task 5c342960-dbd4-4803-8cf0-7fc1876f08f5 to server 0 (has 3 workers)
[162.766] Server 0: Adding new task
[162.766] Creating new TaskEntry with ID: 249c4f91-7041-449a-a033-ab09a29d1d76
[162.766] TaskQueue: Enqueueing task 249c4f91-7041-449a-a033-ab09a29d1d76
[162.767] TaskQueue: Task 249c4f91-7041-449a-a033-ab09a29d1d76 added to pending queue
[162.767] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[162.768] Server 0: Task queued
[162.768] Task 5c342960-dbd4-4803-8cf0-7fc1876f08f5 assigned to server 0
[162.769] UMCG syscall result: 0
[162.769] Server 0: Processing event 1 for worker 4
[162.769] Server 0: Worker 4 blocked
[162.770] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(a7c225c2-df52-46f0-a9fd-8c9f02049a59))
[162.770] Server 0: Attempting to schedule tasks...
[162.770] Server 0: Looking for waiting workers among 3 workers
[162.771] Server 0: No waiting workers found
[162.771] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[162.771] Server 0: Processing event 2 for worker 6
[162.772] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.772] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[162.772] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.773] Server 0: Worker 6 unblocking
[162.773] WorkerPool: Updating worker 2 status from Blocked to Running
[162.773] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[162.774] Server 0: Context switching to worker 6
[162.774] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[162.774] !!!! Initial task 2: WOKE UP FROM SLEEP !!!!
[162.774] !!!! Initial task 2: PREPARING to spawn child task !!!!
[162.775] TaskHandle: Submitting new task
[162.775] Executor: Submitting new task
[162.776] Registered task e9563bd3-2613-4a9c-936e-75c6de31e713, total tasks: 9
[162.776] Executor: Adding task e9563bd3-2613-4a9c-936e-75c6de31e713 to server 0 (has 3 workers)
[162.777] Server 0: Adding new task
[162.777] Creating new TaskEntry with ID: be2784c3-fcca-4ed1-ad55-fb64287bf164
[162.777] TaskQueue: Enqueueing task be2784c3-fcca-4ed1-ad55-fb64287bf164
[162.778] TaskQueue: Task be2784c3-fcca-4ed1-ad55-fb64287bf164 added to pending queue
[162.779] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[162.779] Server 0: Task queued
[162.779] Task e9563bd3-2613-4a9c-936e-75c6de31e713 assigned to server 0
[162.779] UMCG syscall result: 0
[162.780] Server 0: Processing event 1 for worker 6
[162.780] Server 0: Worker 6 blocked
[162.780] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(cc7c0b99-e3da-4f3b-86c4-8bac54d55acc))
[162.780] Server 0: Attempting to schedule tasks...
[162.781] Server 0: Looking for waiting workers among 3 workers
[162.781] Server 0: No waiting workers found
[162.781] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[162.782] Server 0: Attempting to schedule tasks...
[162.782] Server 0: Looking for waiting workers among 3 workers
[162.782] Server 0: No waiting workers found
[162.782] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[162.782] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.783] UMCG syscall result: 0
[162.783] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[162.783] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[162.783] Server 0: Processing event 2 for worker 5
[162.783] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.784] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[162.784] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.784] Server 0: Worker 5 unblocking
[162.784] WorkerPool: Updating worker 1 status from Blocked to Running
[162.785] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[162.785] Server 0: Context switching to worker 5
[162.785] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[162.786] TaskHandle: Task submitted successfully
[162.786] !!!! Initial task 0: COMPLETED !!!!
[162.787] Completed task 3994e194-4d6e-4be9-9d41-0a147f8b78dd, total completed: 1/9
[162.788] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[162.789] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[162.789] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.789] UMCG syscall result: 0
[162.790] Server 0: Processing event 3 for worker 5
[162.790] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[162.790] Server 0: Got explicit WAIT from worker 5
[162.791] Server 0: Worker 5 current status: Running
[162.791] WorkerPool: Updating worker 1 status from Running to Waiting
[162.792] Server 0: Attempting to schedule tasks...
[162.792] Server 0: Looking for waiting workers among 3 workers
[162.792] Server 0: Found waiting worker 1 with status Waiting
[162.792] TaskQueue: Attempting to get next task
[162.793] TaskQueue: Retrieved task ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa
[162.793] Server 0: Assigning task to worker 1
[162.793] WorkerPool: Updating worker 1 status from Waiting to Running
[162.794] TaskQueue: Marking task ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa as in progress with worker 5
[162.794] Worker 1: Starting task assignment
[162.795] Worker 1: Task sent successfully
[162.795] Server 0: Context switching to worker 5
[162.795] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[162.795] UMCG syscall result: 0
[162.796] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[162.797] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[162.797] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[162.797] !!!! Initial task 3: STARTING task !!!!
[162.798] !!!! Initial task 3: ABOUT TO SLEEP !!!!
[162.798] UMCG syscall result: 0
[162.798] Server 0: Processing event 1 for worker 5
[162.798] Server 0: Worker 5 blocked
[162.798] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa))
[162.799] Server 0: Successfully switched to worker 5
[162.799] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[162.799] Server 0: Processing event 2 for worker 4
[162.799] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.800] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[162.800] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.800] Server 0: Worker 4 unblocking
[162.800] WorkerPool: Updating worker 0 status from Blocked to Running
[162.801] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[162.801] Server 0: Context switching to worker 4
[162.801] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[162.801] TaskHandle: Task submitted successfully
[162.802] !!!! Initial task 1: COMPLETED !!!!
[162.802] Completed task 477ca39d-3b4b-4cad-97b7-ffc56db86a54, total completed: 2/9
[162.803] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[162.803] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[162.804] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.804] UMCG syscall result: 0
[162.804] Server 0: Processing event 3 for worker 4
[162.804] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[162.805] Server 0: Got explicit WAIT from worker 4
[162.805] Server 0: Worker 4 current status: Running
[162.805] WorkerPool: Updating worker 0 status from Running to Waiting
[162.806] Server 0: Attempting to schedule tasks...
[162.806] Server 0: Looking for waiting workers among 3 workers
[162.806] Server 0: Found waiting worker 0 with status Waiting
[162.806] TaskQueue: Attempting to get next task
[162.806] TaskQueue: Retrieved task 34be1d03-48ac-4616-b80a-d683c6aa6b17
[162.807] Server 0: Assigning task to worker 0
[162.807] WorkerPool: Updating worker 0 status from Waiting to Running
[162.807] TaskQueue: Marking task 34be1d03-48ac-4616-b80a-d683c6aa6b17 as in progress with worker 4
[162.807] Worker 0: Starting task assignment
[162.808] Worker 0: Task sent successfully
[162.808] Server 0: Context switching to worker 4
[162.808] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[162.808] UMCG syscall result: 0
[162.809] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[162.810] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[162.810] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[162.810] !!!! Initial task 4: STARTING task !!!!
[162.810] !!!! Initial task 4: ABOUT TO SLEEP !!!!
[162.811] UMCG syscall result: 0
[162.811] Server 0: Processing event 1 for worker 4
[162.811] Server 0: Worker 4 blocked
[162.811] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(34be1d03-48ac-4616-b80a-d683c6aa6b17))
[162.812] Server 0: Successfully switched to worker 4
[162.812] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[162.812] Server 0: Attempting to schedule tasks...
[162.812] Server 0: Looking for waiting workers among 3 workers
[162.813] Server 0: No waiting workers found
[162.813] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[162.813] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.813] UMCG syscall result: 0
[162.813] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[162.814] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[162.814] Server 0: Processing event 2 for worker 6
[162.814] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[162.814] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[162.815] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[162.815] Server 0: Worker 6 unblocking
[162.815] WorkerPool: Updating worker 2 status from Blocked to Running
[162.815] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[162.815] Server 0: Context switching to worker 6
[162.815] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[162.816] TaskHandle: Task submitted successfully
[162.816] !!!! Initial task 2: COMPLETED !!!!
[162.816] Completed task d26821db-c873-46f5-80e4-e71ecd35a4f7, total completed: 3/9
[162.816] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[162.816] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[162.817] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[162.817] UMCG syscall result: 0
[162.817] Server 0: Processing event 3 for worker 6
[162.817] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[162.818] Server 0: Got explicit WAIT from worker 6
[162.818] Server 0: Worker 6 current status: Running
[162.818] WorkerPool: Updating worker 2 status from Running to Waiting
[162.818] Server 0: Attempting to schedule tasks...
[162.818] Server 0: Looking for waiting workers among 3 workers
[162.819] Server 0: Found waiting worker 2 with status Waiting
[162.819] TaskQueue: Attempting to get next task
[162.819] TaskQueue: Retrieved task c6d9ffbc-b277-4312-b82d-a6c41108343a
[162.819] Server 0: Assigning task to worker 2
[162.819] WorkerPool: Updating worker 2 status from Waiting to Running
[162.820] TaskQueue: Marking task c6d9ffbc-b277-4312-b82d-a6c41108343a as in progress with worker 6
[162.820] Worker 2: Starting task assignment
[162.820] Worker 2: Task sent successfully
[162.820] Server 0: Context switching to worker 6
[162.820] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[162.821] UMCG syscall result: 0
[162.821] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[162.821] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[162.822] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[162.822] !!!! Initial task 5: STARTING task !!!!
[162.822] !!!! Initial task 5: ABOUT TO SLEEP !!!!
[162.822] UMCG syscall result: 0
[162.823] Server 0: Processing event 1 for worker 6
[162.823] Server 0: Worker 6 blocked
[162.823] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(c6d9ffbc-b277-4312-b82d-a6c41108343a))
[162.823] Server 0: Successfully switched to worker 6
[162.824] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[162.824] Server 0: Attempting to schedule tasks...
[162.824] Server 0: Looking for waiting workers among 3 workers
[162.824] Server 0: No waiting workers found
[162.824] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[162.825] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.808] UMCG syscall result: 0
[164.809] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[164.810] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[164.810] Server 0: Processing event 2 for worker 5
[164.812] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.813] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[164.813] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.814] Server 0: Worker 5 unblocking
[164.815] WorkerPool: Updating worker 1 status from Blocked to Running
[164.815] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[164.815] Server 0: Context switching to worker 5
[164.816] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[164.816] !!!! Initial task 3: WOKE UP FROM SLEEP !!!!
[164.817] !!!! Initial task 3: PREPARING to spawn child task !!!!
[164.817] TaskHandle: Submitting new task
[164.817] Executor: Submitting new task
[164.818] Registered task 69e557b6-0d6a-467b-9c47-43c63fb3051d, total tasks: 10
[164.819] Executor: Adding task 69e557b6-0d6a-467b-9c47-43c63fb3051d to server 0 (has 3 workers)
[164.819] Server 0: Adding new task
[164.819] Creating new TaskEntry with ID: 62174e80-ab36-45bd-ba32-0cf2ea540907
[164.820] TaskQueue: Enqueueing task 62174e80-ab36-45bd-ba32-0cf2ea540907
[164.820] TaskQueue: Task 62174e80-ab36-45bd-ba32-0cf2ea540907 added to pending queue
[164.820] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[164.821] Server 0: Task queued
[164.821] Task 69e557b6-0d6a-467b-9c47-43c63fb3051d assigned to server 0
[164.822] UMCG syscall result: 0
[164.822] Server 0: Processing event 1 for worker 5
[164.822] Server 0: Worker 5 blocked
[164.823] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(ec6bd20d-dd66-430d-a1ce-8ac9c1227aaa))
[164.823] Server 0: Attempting to schedule tasks...
[164.824] Server 0: Looking for waiting workers among 3 workers
[164.824] Server 0: No waiting workers found
[164.825] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[164.825] Server 0: Attempting to schedule tasks...
[164.826] Server 0: Looking for waiting workers among 3 workers
[164.826] Server 0: No waiting workers found
[164.826] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[164.826] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.827] UMCG syscall result: 0
[164.827] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[164.828] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[164.828] Server 0: Processing event 2 for worker 4
[164.828] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.829] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[164.829] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.829] Server 0: Worker 4 unblocking
[164.829] WorkerPool: Updating worker 0 status from Blocked to Running
[164.830] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[164.830] Server 0: Context switching to worker 4
[164.830] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[164.830] !!!! Initial task 4: WOKE UP FROM SLEEP !!!!
[164.830] !!!! Initial task 4: PREPARING to spawn child task !!!!
[164.831] TaskHandle: Submitting new task
[164.831] Executor: Submitting new task
[164.831] Registered task a05f8700-6997-4ea4-8711-326ad8ee3cdc, total tasks: 11
[164.832] Executor: Adding task a05f8700-6997-4ea4-8711-326ad8ee3cdc to server 0 (has 3 workers)
[164.832] Server 0: Adding new task
[164.833] Creating new TaskEntry with ID: 44143609-61fb-4e3f-a8f2-a5741e1b97cb
[164.833] TaskQueue: Enqueueing task 44143609-61fb-4e3f-a8f2-a5741e1b97cb
[164.833] TaskQueue: Task 44143609-61fb-4e3f-a8f2-a5741e1b97cb added to pending queue
[164.834] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[164.834] Server 0: Task queued
[164.834] Task a05f8700-6997-4ea4-8711-326ad8ee3cdc assigned to server 0
[164.835] UMCG syscall result: 0
[164.835] Server 0: Processing event 1 for worker 4
[164.835] Server 0: Worker 4 blocked
[164.836] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(34be1d03-48ac-4616-b80a-d683c6aa6b17))
[164.836] Server 0: Attempting to schedule tasks...
[164.837] Server 0: Looking for waiting workers among 3 workers
[164.837] Server 0: No waiting workers found
[164.837] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[164.837] Server 0: Processing event 2 for worker 6
[164.838] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.838] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[164.839] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.839] Server 0: Worker 6 unblocking
[164.839] WorkerPool: Updating worker 2 status from Blocked to Running
[164.839] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[164.840] Server 0: Context switching to worker 6
[164.840] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[164.841] !!!! Initial task 5: WOKE UP FROM SLEEP !!!!
[164.841] !!!! Initial task 5: PREPARING to spawn child task !!!!
[164.841] TaskHandle: Submitting new task
[164.841] Executor: Submitting new task
[164.842] Registered task df14ed86-307a-4f52-98e8-6b6ac55a3676, total tasks: 12
[164.842] Executor: Adding task df14ed86-307a-4f52-98e8-6b6ac55a3676 to server 0 (has 3 workers)
[164.842] Server 0: Adding new task
[164.842] Creating new TaskEntry with ID: d1706a60-daa0-4da7-93fc-1985e979c318
[164.843] TaskQueue: Enqueueing task d1706a60-daa0-4da7-93fc-1985e979c318
[164.843] TaskQueue: Task d1706a60-daa0-4da7-93fc-1985e979c318 added to pending queue
[164.844] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[164.844] Server 0: Task queued
[164.844] Task df14ed86-307a-4f52-98e8-6b6ac55a3676 assigned to server 0
[164.845] UMCG syscall result: 0
[164.845] Server 0: Processing event 1 for worker 6
[164.845] Server 0: Worker 6 blocked
[164.846] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(c6d9ffbc-b277-4312-b82d-a6c41108343a))
[164.846] Server 0: Attempting to schedule tasks...
[164.847] Server 0: Looking for waiting workers among 3 workers
[164.847] Server 0: No waiting workers found
[164.847] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[164.847] Server 0: Attempting to schedule tasks...
[164.847] Server 0: Looking for waiting workers among 3 workers
[164.848] Server 0: No waiting workers found
[164.848] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[164.848] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.849] UMCG syscall result: 0
[164.849] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[164.849] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[164.850] Server 0: Processing event 2 for worker 5
[164.850] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.850] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[164.851] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.851] Server 0: Worker 5 unblocking
[164.851] WorkerPool: Updating worker 1 status from Blocked to Running
[164.851] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[164.852] Server 0: Context switching to worker 5
[164.852] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[164.852] TaskHandle: Task submitted successfully
[164.852] !!!! Initial task 3: COMPLETED !!!!
[164.853] Completed task 22483a39-592c-4576-8661-d751cf3dcdcb, total completed: 4/12
[164.853] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[164.854] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[164.854] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.854] UMCG syscall result: 0
[164.854] Server 0: Processing event 3 for worker 5
[164.854] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[164.855] Server 0: Got explicit WAIT from worker 5
[164.855] Server 0: Worker 5 current status: Running
[164.855] WorkerPool: Updating worker 1 status from Running to Waiting
[164.855] Server 0: Attempting to schedule tasks...
[164.856] Server 0: Looking for waiting workers among 3 workers
[164.856] Server 0: Found waiting worker 1 with status Waiting
[164.856] TaskQueue: Attempting to get next task
[164.856] TaskQueue: Retrieved task b7323d59-f3ce-4daf-a607-215c10ed08ae
[164.857] Server 0: Assigning task to worker 1
[164.857] WorkerPool: Updating worker 1 status from Waiting to Running
[164.857] TaskQueue: Marking task b7323d59-f3ce-4daf-a607-215c10ed08ae as in progress with worker 5
[164.857] Worker 1: Starting task assignment
[164.858] Worker 1: Task sent successfully
[164.858] Server 0: Context switching to worker 5
[164.858] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[164.858] UMCG syscall result: 0
[164.859] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[164.859] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[164.860] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[164.860] !!!! Child task of initial task 0: STARTING work !!!!
[164.861] UMCG syscall result: 0
[164.861] Server 0: Processing event 1 for worker 5
[164.861] Server 0: Worker 5 blocked
[164.861] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(b7323d59-f3ce-4daf-a607-215c10ed08ae))
[164.862] Server 0: Successfully switched to worker 5
[164.862] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[164.862] Server 0: Processing event 2 for worker 4
[164.862] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.863] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[164.863] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.863] Server 0: Worker 4 unblocking
[164.863] WorkerPool: Updating worker 0 status from Blocked to Running
[164.864] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[164.864] Server 0: Context switching to worker 4
[164.864] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[164.864] TaskHandle: Task submitted successfully
[164.865] !!!! Initial task 4: COMPLETED !!!!
[164.865] Completed task 43707f4d-2013-4b0b-bcb7-812cf4f38474, total completed: 5/12
[164.865] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[164.865] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[164.866] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.866] UMCG syscall result: 0
[164.866] Server 0: Processing event 3 for worker 4
[164.866] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[164.867] Server 0: Got explicit WAIT from worker 4
[164.867] Server 0: Worker 4 current status: Running
[164.867] WorkerPool: Updating worker 0 status from Running to Waiting
[164.867] Server 0: Attempting to schedule tasks...
[164.868] Server 0: Looking for waiting workers among 3 workers
[164.868] Server 0: Found waiting worker 0 with status Waiting
[164.868] TaskQueue: Attempting to get next task
[164.868] TaskQueue: Retrieved task 249c4f91-7041-449a-a033-ab09a29d1d76
[164.869] Server 0: Assigning task to worker 0
[164.869] WorkerPool: Updating worker 0 status from Waiting to Running
[164.869] TaskQueue: Marking task 249c4f91-7041-449a-a033-ab09a29d1d76 as in progress with worker 4
[164.869] Worker 0: Starting task assignment
[164.870] Worker 0: Task sent successfully
[164.870] Server 0: Context switching to worker 4
[164.870] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[164.870] UMCG syscall result: 0
[164.871] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[164.871] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[164.871] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[164.872] !!!! Child task of initial task 1: STARTING work !!!!
[164.872] UMCG syscall result: 0
[164.872] Server 0: Processing event 1 for worker 4
[164.872] Server 0: Worker 4 blocked
[164.873] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(249c4f91-7041-449a-a033-ab09a29d1d76))
[164.873] Server 0: Successfully switched to worker 4
[164.874] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[164.874] Server 0: Attempting to schedule tasks...
[164.874] Server 0: Looking for waiting workers among 3 workers
[164.874] Server 0: No waiting workers found
[164.875] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[164.875] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.875] UMCG syscall result: 0
[164.875] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[164.875] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[164.875] Server 0: Processing event 2 for worker 6
[164.876] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[164.876] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[164.876] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[164.876] Server 0: Worker 6 unblocking
[164.876] WorkerPool: Updating worker 2 status from Blocked to Running
[164.877] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[164.877] Server 0: Context switching to worker 6
[164.877] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[164.877] TaskHandle: Task submitted successfully
[164.877] !!!! Initial task 5: COMPLETED !!!!
[164.878] Completed task 20d86a84-3a93-4a2f-a079-788aa3fb3fac, total completed: 6/12
[164.878] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[164.878] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[164.878] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[164.879] UMCG syscall result: 0
[164.879] Server 0: Processing event 3 for worker 6
[164.879] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[164.879] Server 0: Got explicit WAIT from worker 6
[164.879] Server 0: Worker 6 current status: Running
[164.880] WorkerPool: Updating worker 2 status from Running to Waiting
[164.880] Server 0: Attempting to schedule tasks...
[164.880] Server 0: Looking for waiting workers among 3 workers
[164.880] Server 0: Found waiting worker 2 with status Waiting
[164.881] TaskQueue: Attempting to get next task
[164.881] TaskQueue: Retrieved task be2784c3-fcca-4ed1-ad55-fb64287bf164
[164.881] Server 0: Assigning task to worker 2
[164.881] WorkerPool: Updating worker 2 status from Waiting to Running
[164.881] TaskQueue: Marking task be2784c3-fcca-4ed1-ad55-fb64287bf164 as in progress with worker 6
[164.882] Worker 2: Starting task assignment
[164.882] Worker 2: Task sent successfully
[164.882] Server 0: Context switching to worker 6
[164.882] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[164.882] UMCG syscall result: 0
[164.883] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[164.883] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[164.883] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[164.884] !!!! Child task of initial task 2: STARTING work !!!!
[164.884] UMCG syscall result: 0
[164.884] Server 0: Processing event 1 for worker 6
[164.884] Server 0: Worker 6 blocked
[164.885] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(be2784c3-fcca-4ed1-ad55-fb64287bf164))
[164.885] Server 0: Successfully switched to worker 6
[164.885] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[164.885] Server 0: Attempting to schedule tasks...
[164.885] Server 0: Looking for waiting workers among 3 workers
[164.886] Server 0: No waiting workers found
[164.886] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[164.886] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.719] Progress: 6/12 tasks completed
[165.831] Progress: 6/12 tasks completed
[165.869] UMCG syscall result: 0
[165.870] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[165.871] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[165.872] Server 0: Processing event 2 for worker 5
[165.873] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[165.875] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[165.876] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[165.878] Server 0: Worker 5 unblocking
[165.878] WorkerPool: Updating worker 1 status from Blocked to Running
[165.879] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[165.880] Server 0: Context switching to worker 5
[165.880] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[165.881] !!!! Child task of initial task 0: COMPLETED !!!!
[165.882] Completed task 02d11542-801c-4c9e-9b62-bb5527d83dff, total completed: 7/12
[165.883] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[165.884] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[165.884] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.885] UMCG syscall result: 0
[165.885] Server 0: Processing event 3 for worker 5
[165.886] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[165.886] Server 0: Got explicit WAIT from worker 5
[165.886] Server 0: Worker 5 current status: Running
[165.886] WorkerPool: Updating worker 1 status from Running to Waiting
[165.887] Server 0: Attempting to schedule tasks...
[165.887] Server 0: Looking for waiting workers among 3 workers
[165.887] Server 0: Found waiting worker 1 with status Waiting
[165.888] TaskQueue: Attempting to get next task
[165.889] TaskQueue: Retrieved task 62174e80-ab36-45bd-ba32-0cf2ea540907
[165.889] Server 0: Assigning task to worker 1
[165.889] WorkerPool: Updating worker 1 status from Waiting to Running
[165.890] TaskQueue: Marking task 62174e80-ab36-45bd-ba32-0cf2ea540907 as in progress with worker 5
[165.890] Worker 1: Starting task assignment
[165.891] Worker 1: Task sent successfully
[165.891] Server 0: Context switching to worker 5
[165.891] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[165.891] UMCG syscall result: 0
[165.892] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[165.892] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[165.893] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[165.893] !!!! Child task of initial task 3: STARTING work !!!!
[165.893] UMCG syscall result: 0
[165.893] Server 0: Processing event 1 for worker 5
[165.894] Server 0: Worker 5 blocked
[165.894] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(62174e80-ab36-45bd-ba32-0cf2ea540907))
[165.895] Server 0: Successfully switched to worker 5
[165.895] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[165.896] Server 0: Attempting to schedule tasks...
[165.896] Server 0: Looking for waiting workers among 3 workers
[165.896] Server 0: No waiting workers found
[165.897] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[165.897] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.898] UMCG syscall result: 0
[165.898] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[165.898] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[165.898] Server 0: Processing event 2 for worker 4
[165.899] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[165.899] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[165.899] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[165.900] Server 0: Worker 4 unblocking
[165.900] WorkerPool: Updating worker 0 status from Blocked to Running
[165.900] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[165.901] Server 0: Context switching to worker 4
[165.901] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[165.901] !!!! Child task of initial task 1: COMPLETED !!!!
[165.902] Completed task 5c342960-dbd4-4803-8cf0-7fc1876f08f5, total completed: 8/12
[165.902] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[165.902] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[165.903] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.903] UMCG syscall result: 0
[165.903] Server 0: Processing event 3 for worker 4
[165.903] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[165.904] Server 0: Got explicit WAIT from worker 4
[165.904] Server 0: Worker 4 current status: Running
[165.904] WorkerPool: Updating worker 0 status from Running to Waiting
[165.904] Server 0: Attempting to schedule tasks...
[165.905] Server 0: Looking for waiting workers among 3 workers
[165.905] Server 0: Found waiting worker 0 with status Waiting
[165.905] TaskQueue: Attempting to get next task
[165.906] TaskQueue: Retrieved task 44143609-61fb-4e3f-a8f2-a5741e1b97cb
[165.906] Server 0: Assigning task to worker 0
[165.906] WorkerPool: Updating worker 0 status from Waiting to Running
[165.906] TaskQueue: Marking task 44143609-61fb-4e3f-a8f2-a5741e1b97cb as in progress with worker 4
[165.907] Worker 0: Starting task assignment
[165.907] Worker 0: Task sent successfully
[165.907] Server 0: Context switching to worker 4
[165.908] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[165.908] UMCG syscall result: 0
[165.908] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[165.908] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[165.909] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[165.910] !!!! Child task of initial task 4: STARTING work !!!!
[165.910] UMCG syscall result: 0
[165.910] Server 0: Processing event 1 for worker 4
[165.910] Server 0: Worker 4 blocked
[165.911] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(44143609-61fb-4e3f-a8f2-a5741e1b97cb))
[165.911] Server 0: Successfully switched to worker 4
[165.911] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[165.911] Server 0: Processing event 2 for worker 6
[165.912] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[165.912] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[165.912] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[165.913] Server 0: Worker 6 unblocking
[165.913] WorkerPool: Updating worker 2 status from Blocked to Running
[165.913] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[165.914] Server 0: Context switching to worker 6
[165.914] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[165.914] !!!! Child task of initial task 2: COMPLETED !!!!
[165.915] Completed task e9563bd3-2613-4a9c-936e-75c6de31e713, total completed: 9/12
[165.915] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[165.915] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[165.916] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.916] UMCG syscall result: 0
[165.916] Server 0: Processing event 3 for worker 6
[165.916] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[165.916] Server 0: Got explicit WAIT from worker 6
[165.917] Server 0: Worker 6 current status: Running
[165.917] WorkerPool: Updating worker 2 status from Running to Waiting
[165.917] Server 0: Attempting to schedule tasks...
[165.917] Server 0: Looking for waiting workers among 3 workers
[165.917] Server 0: Found waiting worker 2 with status Waiting
[165.918] TaskQueue: Attempting to get next task
[165.918] TaskQueue: Retrieved task d1706a60-daa0-4da7-93fc-1985e979c318
[165.919] Server 0: Assigning task to worker 2
[165.919] WorkerPool: Updating worker 2 status from Waiting to Running
[165.919] TaskQueue: Marking task d1706a60-daa0-4da7-93fc-1985e979c318 as in progress with worker 6
[165.919] Worker 2: Starting task assignment
[165.920] Worker 2: Task sent successfully
[165.920] Server 0: Context switching to worker 6
[165.920] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[165.920] UMCG syscall result: 0
[165.921] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[165.921] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[165.921] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[165.922] !!!! Child task of initial task 5: STARTING work !!!!
[165.922] UMCG syscall result: 0
[165.922] Server 0: Processing event 1 for worker 6
[165.922] Server 0: Worker 6 blocked
[165.923] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(d1706a60-daa0-4da7-93fc-1985e979c318))
[165.923] Server 0: Successfully switched to worker 6
[165.923] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[165.924] Server 0: Attempting to schedule tasks...
[165.924] Server 0: Looking for waiting workers among 3 workers
[165.924] Server 0: No waiting workers found
[165.924] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[165.924] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[165.935] Progress: 9/12 tasks completed
[166.039] Progress: 9/12 tasks completed
[166.146] Progress: 9/12 tasks completed
[166.253] Progress: 9/12 tasks completed
[166.360] Progress: 9/12 tasks completed
[166.469] Progress: 9/12 tasks completed
[166.573] Progress: 9/12 tasks completed
[166.679] Progress: 9/12 tasks completed
[166.896] UMCG syscall result: 0
[166.897] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[166.898] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[166.899] Server 0: Processing event 2 for worker 5
[166.899] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[166.900] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[166.901] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[166.901] Server 0: Worker 5 unblocking
[166.903] WorkerPool: Updating worker 1 status from Blocked to Running
[166.903] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[166.904] Server 0: Context switching to worker 5
[166.904] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[166.906] !!!! Child task of initial task 3: COMPLETED !!!!
[166.907] Completed task 69e557b6-0d6a-467b-9c47-43c63fb3051d, total completed: 10/12
[166.907] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[166.908] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[166.908] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.908] UMCG syscall result: 0
[166.908] Server 0: Processing event 3 for worker 5
[166.909] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[166.909] Server 0: Got explicit WAIT from worker 5
[166.910] Server 0: Worker 5 current status: Running
[166.910] WorkerPool: Updating worker 1 status from Running to Waiting
[166.911] Server 0: Attempting to schedule tasks...
[166.911] Server 0: Looking for waiting workers among 3 workers
[166.912] Server 0: Found waiting worker 1 with status Waiting
[166.912] TaskQueue: Attempting to get next task
[166.913] TaskQueue: No tasks available
[166.913] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[166.914] Server 0: Attempting to schedule tasks...
[166.914] Server 0: Looking for waiting workers among 3 workers
[166.915] Server 0: Found waiting worker 1 with status Waiting
[166.916] TaskQueue: Attempting to get next task
[166.916] TaskQueue: No tasks available
[166.916] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[166.917] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.917] UMCG syscall result: 0
[166.917] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[166.918] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[166.918] Server 0: Processing event 2 for worker 4
[166.919] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[166.919] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[166.919] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[166.919] Server 0: Worker 4 unblocking
[166.920] WorkerPool: Updating worker 0 status from Blocked to Running
[166.920] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[166.920] Server 0: Context switching to worker 4
[166.921] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[166.921] !!!! Child task of initial task 4: COMPLETED !!!!
[166.921] Completed task a05f8700-6997-4ea4-8711-326ad8ee3cdc, total completed: 11/12
[166.922] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[166.922] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[166.922] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.923] UMCG syscall result: 0
[166.923] Server 0: Processing event 3 for worker 4
[166.924] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[166.924] Server 0: Got explicit WAIT from worker 4
[166.924] Server 0: Worker 4 current status: Running
[166.924] WorkerPool: Updating worker 0 status from Running to Waiting
[166.925] Server 0: Attempting to schedule tasks...
[166.925] Server 0: Looking for waiting workers among 3 workers
[166.925] Server 0: Found waiting worker 1 with status Waiting
[166.926] TaskQueue: Attempting to get next task
[166.926] TaskQueue: No tasks available
[166.926] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[166.927] Server 0: Attempting to schedule tasks...
[166.927] Server 0: Looking for waiting workers among 3 workers
[166.927] Server 0: Found waiting worker 1 with status Waiting
[166.928] TaskQueue: Attempting to get next task
[166.928] TaskQueue: No tasks available
[166.928] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[166.928] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.929] UMCG syscall result: 0
[166.929] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[166.929] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[166.929] Server 0: Processing event 2 for worker 6
[166.930] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[166.930] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[166.931] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[166.931] Server 0: Worker 6 unblocking
[166.931] WorkerPool: Updating worker 2 status from Blocked to Running
[166.931] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[166.932] Server 0: Context switching to worker 6
[166.932] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[166.933] !!!! Child task of initial task 5: COMPLETED !!!!
[166.933] Completed task df14ed86-307a-4f52-98e8-6b6ac55a3676, total completed: 12/12
[166.933] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[166.934] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[166.934] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.934] UMCG syscall result: 0
[166.935] Server 0: Processing event 3 for worker 6
[166.935] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[166.935] Server 0: Got explicit WAIT from worker 6
[166.935] Server 0: Worker 6 current status: Running
[166.935] WorkerPool: Updating worker 2 status from Running to Waiting
[166.935] Server 0: Attempting to schedule tasks...
[166.936] Server 0: Looking for waiting workers among 3 workers
[166.936] Server 0: Found waiting worker 1 with status Waiting
[166.936] TaskQueue: Attempting to get next task
[166.936] TaskQueue: No tasks available
[166.937] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[166.937] Server 0: Attempting to schedule tasks...
[166.937] Server 0: Looking for waiting workers among 3 workers
[166.938] Server 0: Found waiting worker 1 with status Waiting
[166.938] TaskQueue: Attempting to get next task
[166.939] TaskQueue: No tasks available
[166.939] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[166.939] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[166.992] All tasks completed successfully (12/12)
[166.992] Initiating executor shutdown...
[166.993] Server 0: Adding new task
[166.993] Server 0: Received shutdown signal
Running run_multi_server_demo
➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[208.469] Creating new Executor
[208.486] Creating Server 0 on CPU 0
[208.493] Creating Server 0
[208.495] Creating new TaskQueue
[208.498] Creating WorkerPool with capacity for 3 workers
[208.500] Starting executor...
[208.501] Executor: Starting servers
[208.522] Executor: All servers started
[208.535] Successfully set CPU affinity to 0
[208.544] Server 0: Starting up
[208.546] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[208.550] UMCG syscall result: 0
[208.551] Server 0: UMCG registration complete
[208.552] Server 0: Initializing workers
[208.552] Server 0: Initializing worker 0
[208.555] Starting WorkerThread 0 for server 0
[208.566] Successfully set CPU affinity to 0
[208.567] Worker 0: Initialized with tid 4
[208.569] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[208.571] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[208.575] WorkerThread 0 started with tid 4 for server 0
[208.576] Creating new Worker 0 for server 0
[208.577] WorkerPool: Adding worker 0 with tid 4
[208.579] WorkerPool: Updating worker 0 status from Initializing to Registering
[208.581] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[208.581] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[208.588] UMCG syscall result: 0
[208.588] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[208.589] Submitting initial tasks...
[208.592] !!!! Submitting initial task 0 !!!!
[208.592] Executor: Submitting new task
[208.591] Worker 0: Registering worker event 130
[208.607] WorkerPool: Updating worker 0 status from Registering to Waiting
[208.607] Registered task 24cec5ef-0c32-4f5d-a508-c72f77926d6f, total tasks: 1
[208.608] Server 0: Worker 0 initialized successfully
[208.610] Server 0: Initializing worker 1
[208.611] Starting WorkerThread 1 for server 0
[208.610] Executor: Adding task 24cec5ef-0c32-4f5d-a508-c72f77926d6f to server 0 (has 1 workers)
[208.612] Server 0: Adding new task
[208.618] Successfully set CPU affinity to 0
[208.619] Worker 1: Initialized with tid 5
[208.615] Creating new TaskEntry with ID: 9ad83bf6-1e43-47c0-b67c-fe5b9f36febf
[208.624] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[208.626] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[208.626] WorkerThread 1 started with tid 5 for server 0
[208.625] TaskQueue: Enqueueing task 9ad83bf6-1e43-47c0-b67c-fe5b9f36febf
[208.628] Creating new Worker 1 for server 0
[208.629] WorkerPool: Adding worker 1 with tid 5
[208.628] TaskQueue: Task 9ad83bf6-1e43-47c0-b67c-fe5b9f36febf added to pending queue
[208.631] WorkerPool: Updating worker 1 status from Initializing to Registering
[208.631] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[208.630] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[208.632] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[208.638] UMCG syscall result: 0
[208.636] Server 0: Task queued
[208.638] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[208.640] Worker 0: Registering worker event 162
[208.640] WorkerPool: Updating worker 1 status from Registering to Waiting
[208.640] Server 0: Worker 1 initialized successfully
[208.641] Server 0: Initializing worker 2
[208.639] Task 24cec5ef-0c32-4f5d-a508-c72f77926d6f assigned to server 0
[208.644] !!!! Submitting initial task 1 !!!!
[208.644] Executor: Submitting new task
[208.645] Registered task c9817158-04f0-49df-9c43-6e75bb379272, total tasks: 2
[208.643] Starting WorkerThread 2 for server 0
[208.648] Successfully set CPU affinity to 0
[208.648] Worker 2: Initialized with tid 6
[208.646] Executor: Adding task c9817158-04f0-49df-9c43-6e75bb379272 to server 0 (has 2 workers)
[208.649] Server 0: Adding new task
[208.650] Creating new TaskEntry with ID: 1de00e23-9cea-4766-a256-4c1b4de0256b
[208.649] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[208.652] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[208.653] WorkerThread 2 started with tid 6 for server 0
[208.654] TaskQueue: Enqueueing task 1de00e23-9cea-4766-a256-4c1b4de0256b
[208.655] TaskQueue: Task 1de00e23-9cea-4766-a256-4c1b4de0256b added to pending queue
[208.654] Creating new Worker 2 for server 0
[208.655] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[208.656] WorkerPool: Adding worker 2 with tid 6
[208.657] WorkerPool: Updating worker 2 status from Initializing to Registering
[208.657] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[208.656] Server 0: Task queued
[208.657] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[208.660] UMCG syscall result: 0
[208.660] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[208.659] Task c9817158-04f0-49df-9c43-6e75bb379272 assigned to server 0
[208.661] Worker 0: Registering worker event 194
[208.661] WorkerPool: Updating worker 2 status from Registering to Waiting
[208.661] Server 0: Worker 2 initialized successfully
[208.661] !!!! Submitting initial task 2 !!!!
[208.661] Executor: Submitting new task
[208.661] Server 0: All workers initialized
[208.662] Registered task 93a0e583-a197-49d9-a0b4-b8c946f520f1, total tasks: 3
[208.662] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[208.663] Server 0: Attempting to schedule tasks...
[208.663] Executor: Adding task 93a0e583-a197-49d9-a0b4-b8c946f520f1 to server 0 (has 3 workers)
[208.664] Server 0: Adding new task
[208.665] Creating new TaskEntry with ID: c3b1dd8c-bc64-48f9-9420-bfe34d1b2258
[208.664] Server 0: Looking for waiting workers among 3 workers
[208.665] TaskQueue: Enqueueing task c3b1dd8c-bc64-48f9-9420-bfe34d1b2258
[208.666] TaskQueue: Task c3b1dd8c-bc64-48f9-9420-bfe34d1b2258 added to pending queue
[208.666] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[208.666] Server 0: Found waiting worker 1 with status Waiting
[208.667] Server 0: Task queued
[208.667] Task 93a0e583-a197-49d9-a0b4-b8c946f520f1 assigned to server 0
[208.668] !!!! Submitting initial task 3 !!!!
[208.668] Executor: Submitting new task
[208.669] TaskQueue: Attempting to get next task
[208.670] TaskQueue: Retrieved task 9ad83bf6-1e43-47c0-b67c-fe5b9f36febf
[208.669] Registered task de0699ef-61dd-4462-a6cc-73afd8b4c5ca, total tasks: 4
[208.671] Server 0: Assigning task to worker 1
[208.671] Executor: Adding task de0699ef-61dd-4462-a6cc-73afd8b4c5ca to server 0 (has 3 workers)
[208.672] WorkerPool: Updating worker 1 status from Waiting to Running
[208.672] Server 0: Adding new task
[208.672] Creating new TaskEntry with ID: db803747-541a-44d8-9d51-1f5bbe3a1a5d
[208.673] TaskQueue: Marking task 9ad83bf6-1e43-47c0-b67c-fe5b9f36febf as in progress with worker 5
[208.673] TaskQueue: Enqueueing task db803747-541a-44d8-9d51-1f5bbe3a1a5d
[208.674] TaskQueue: Task db803747-541a-44d8-9d51-1f5bbe3a1a5d added to pending queue
[208.674] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[208.674] Worker 1: Starting task assignment
[208.675] Server 0: Task queued
[208.675] Task de0699ef-61dd-4462-a6cc-73afd8b4c5ca assigned to server 0
[208.675] !!!! Submitting initial task 4 !!!!
[208.676] Executor: Submitting new task
[208.677] Registered task 0b6825d4-de03-4533-a0a8-132b554f5581, total tasks: 5
[208.676] Worker 1: Task sent successfully
[208.678] Executor: Adding task 0b6825d4-de03-4533-a0a8-132b554f5581 to server 0 (has 3 workers)
[208.679] Server 0: Adding new task
[208.679] Creating new TaskEntry with ID: b3480109-806d-4472-b766-8acbf6008f23
[208.678] Server 0: Context switching to worker 5
[208.680] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[208.679] TaskQueue: Enqueueing task b3480109-806d-4472-b766-8acbf6008f23
[208.683] TaskQueue: Task b3480109-806d-4472-b766-8acbf6008f23 added to pending queue
[208.684] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[208.686] Server 0: Task queued
[208.687] UMCG syscall result: 0
[208.687] Task 0b6825d4-de03-4533-a0a8-132b554f5581 assigned to server 0
[208.687] !!!! Submitting initial task 5 !!!!
[208.687] Server 0: Processing event 1 for worker 5
[208.688] Executor: Submitting new task
[208.688] Server 0: Worker 5 blocked
[208.689] Registered task b6c72c64-a201-48e6-bb8e-399a9d2bbee1, total tasks: 6
[208.691] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(9ad83bf6-1e43-47c0-b67c-fe5b9f36febf))
[208.692] Executor: Adding task b6c72c64-a201-48e6-bb8e-399a9d2bbee1 to server 0 (has 3 workers)
[208.692] Server 0: Successfully switched to worker 5
[208.693] Server 0: Adding new task
[208.693] Creating new TaskEntry with ID: 07f09dd3-6985-41b0-96ec-f492ded43f61
[208.694] TaskQueue: Enqueueing task 07f09dd3-6985-41b0-96ec-f492ded43f61
[208.694] TaskQueue: Task 07f09dd3-6985-41b0-96ec-f492ded43f61 added to pending queue
[208.695] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[208.695] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[208.696] UMCG syscall result: 0
[208.697] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[208.697] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[208.698] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[208.698] Server 0: Task queued
[208.698] Server 0: Processing event 2 for worker 5
[208.698] Task b6c72c64-a201-48e6-bb8e-399a9d2bbee1 assigned to server 0
[208.699] All tasks submitted, waiting for completion...
[208.699] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[208.702] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[208.703] Progress: 0/6 tasks completed
[208.703] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[208.705] Server 0: Worker 5 unblocking
[208.706] WorkerPool: Updating worker 1 status from Blocked to Running
[208.706] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[208.707] Server 0: Context switching to worker 5
[208.707] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[208.709] UMCG syscall result: 0
[208.709] Worker 1: UMCG registration complete with server 0
[208.710] Worker 1: Entering task processing loop
[208.711] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[208.711] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[208.711] !!!! Initial task 0: STARTING task !!!!
[208.712] !!!! Initial task 0: ABOUT TO SLEEP !!!!
[208.713] UMCG syscall result: 0
[208.713] Server 0: Processing event 1 for worker 5
[208.713] Server 0: Worker 5 blocked
[208.714] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(9ad83bf6-1e43-47c0-b67c-fe5b9f36febf))
[208.715] Server 0: Attempting to schedule tasks...
[208.715] Server 0: Looking for waiting workers among 3 workers
[208.716] Server 0: Found waiting worker 0 with status Waiting
[208.716] TaskQueue: Attempting to get next task
[208.717] TaskQueue: Retrieved task 1de00e23-9cea-4766-a256-4c1b4de0256b
[208.717] Server 0: Assigning task to worker 0
[208.717] WorkerPool: Updating worker 0 status from Waiting to Running
[208.718] TaskQueue: Marking task 1de00e23-9cea-4766-a256-4c1b4de0256b as in progress with worker 4
[208.718] Worker 0: Starting task assignment
[208.719] Worker 0: Task sent successfully
[208.719] Server 0: Context switching to worker 4
[208.720] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[208.720] UMCG syscall result: 0
[208.720] Worker 0: UMCG registration complete with server 0
[208.721] Worker 0: Entering task processing loop
[208.721] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[208.722] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[208.722] !!!! Initial task 1: STARTING task !!!!
[208.723] !!!! Initial task 1: ABOUT TO SLEEP !!!!
[208.723] UMCG syscall result: 0
[208.723] Server 0: Processing event 1 for worker 4
[208.723] Server 0: Worker 4 blocked
[208.724] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(1de00e23-9cea-4766-a256-4c1b4de0256b))
[208.724] Server 0: Successfully switched to worker 4
[208.725] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[208.725] Server 0: Attempting to schedule tasks...
[208.725] Server 0: Looking for waiting workers among 3 workers
[208.725] Server 0: Found waiting worker 2 with status Waiting
[208.726] TaskQueue: Attempting to get next task
[208.726] TaskQueue: Retrieved task c3b1dd8c-bc64-48f9-9420-bfe34d1b2258
[208.726] Server 0: Assigning task to worker 2
[208.727] WorkerPool: Updating worker 2 status from Waiting to Running
[208.727] TaskQueue: Marking task c3b1dd8c-bc64-48f9-9420-bfe34d1b2258 as in progress with worker 6
[208.727] Worker 2: Starting task assignment
[208.728] Worker 2: Task sent successfully
[208.728] Server 0: Context switching to worker 6
[208.728] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[208.728] UMCG syscall result: 0
[208.729] Worker 2: UMCG registration complete with server 0
[208.729] Worker 2: Entering task processing loop
[208.729] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[208.730] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[208.730] !!!! Initial task 2: STARTING task !!!!
[208.731] !!!! Initial task 2: ABOUT TO SLEEP !!!!
[208.731] UMCG syscall result: 0
[208.731] Server 0: Processing event 1 for worker 6
[208.731] Server 0: Worker 6 blocked
[208.732] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(c3b1dd8c-bc64-48f9-9420-bfe34d1b2258))
[208.732] Server 0: Successfully switched to worker 6
[208.733] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[208.733] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[208.814] Progress: 0/6 tasks completed
[208.925] Progress: 0/6 tasks completed
[209.044] Progress: 0/6 tasks completed
[209.147] Progress: 0/6 tasks completed
[209.251] Progress: 0/6 tasks completed
[209.357] Progress: 0/6 tasks completed
[209.461] Progress: 0/6 tasks completed
[209.571] Progress: 0/6 tasks completed
[209.676] Progress: 0/6 tasks completed
en1: assigned FE80::2098:F9FF:FE3F:FEA1
[210.733] UMCG syscall result: 0
[210.736] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[210.737] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[210.738] Server 0: Processing event 2 for worker 5
[210.738] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.740] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[210.741] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.742] Server 0: Worker 5 unblocking
[210.742] WorkerPool: Updating worker 1 status from Blocked to Running
[210.743] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[210.743] Server 0: Context switching to worker 5
[210.744] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[210.744] !!!! Initial task 0: WOKE UP FROM SLEEP !!!!
[210.745] !!!! Initial task 0: PREPARING to spawn child task !!!!
[210.746] TaskHandle: Submitting new task
[210.746] Executor: Submitting new task
[210.748] Registered task bcb73d2a-bddc-43a2-9404-e8c711673b5c, total tasks: 7
[210.750] Executor: Adding task bcb73d2a-bddc-43a2-9404-e8c711673b5c to server 0 (has 3 workers)
[210.750] Server 0: Adding new task
[210.752] Creating new TaskEntry with ID: 6d67623e-c1a1-47ed-abe4-7a428e9b7492
[210.752] TaskQueue: Enqueueing task 6d67623e-c1a1-47ed-abe4-7a428e9b7492
[210.753] TaskQueue: Task 6d67623e-c1a1-47ed-abe4-7a428e9b7492 added to pending queue
[210.754] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[210.754] Server 0: Task queued
[210.755] Task bcb73d2a-bddc-43a2-9404-e8c711673b5c assigned to server 0
[210.755] UMCG syscall result: 0
[210.756] Server 0: Processing event 1 for worker 5
[210.756] Server 0: Worker 5 blocked
[210.757] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(9ad83bf6-1e43-47c0-b67c-fe5b9f36febf))
[210.758] Server 0: Attempting to schedule tasks...
[210.759] Server 0: Looking for waiting workers among 3 workers
[210.759] Server 0: No waiting workers found
[210.760] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[210.760] Server 0: Attempting to schedule tasks...
[210.760] Server 0: Looking for waiting workers among 3 workers
[210.761] Server 0: No waiting workers found
[210.761] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[210.761] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.762] UMCG syscall result: 0
[210.762] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[210.762] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[210.763] Server 0: Processing event 2 for worker 4
[210.763] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.764] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[210.764] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.764] Server 0: Worker 4 unblocking
[210.765] WorkerPool: Updating worker 0 status from Blocked to Running
[210.765] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[210.766] Server 0: Context switching to worker 4
[210.766] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[210.766] !!!! Initial task 1: WOKE UP FROM SLEEP !!!!
[210.767] !!!! Initial task 1: PREPARING to spawn child task !!!!
[210.767] TaskHandle: Submitting new task
[210.768] Executor: Submitting new task
[210.769] Registered task 65a2e234-4ab4-4cdc-9280-2d3e94f8c1d0, total tasks: 8
[210.770] Executor: Adding task 65a2e234-4ab4-4cdc-9280-2d3e94f8c1d0 to server 0 (has 3 workers)
[210.770] Server 0: Adding new task
[210.770] Creating new TaskEntry with ID: 22e58ebb-d9d8-4a7a-9106-38ffe2daba16
[210.771] TaskQueue: Enqueueing task 22e58ebb-d9d8-4a7a-9106-38ffe2daba16
[210.771] TaskQueue: Task 22e58ebb-d9d8-4a7a-9106-38ffe2daba16 added to pending queue
[210.772] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[210.772] Server 0: Task queued
[210.773] Task 65a2e234-4ab4-4cdc-9280-2d3e94f8c1d0 assigned to server 0
[210.773] UMCG syscall result: 0
[210.774] Server 0: Processing event 1 for worker 4
[210.774] Server 0: Worker 4 blocked
[210.774] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(1de00e23-9cea-4766-a256-4c1b4de0256b))
[210.775] Server 0: Attempting to schedule tasks...
[210.775] Server 0: Looking for waiting workers among 3 workers
[210.776] Server 0: No waiting workers found
[210.776] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[210.776] Server 0: Processing event 2 for worker 6
[210.776] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.777] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[210.777] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.777] Server 0: Worker 6 unblocking
[210.777] WorkerPool: Updating worker 2 status from Blocked to Running
[210.778] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[210.778] Server 0: Context switching to worker 6
[210.778] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[210.779] !!!! Initial task 2: WOKE UP FROM SLEEP !!!!
[210.779] !!!! Initial task 2: PREPARING to spawn child task !!!!
[210.779] TaskHandle: Submitting new task
[210.779] Executor: Submitting new task
[210.780] Registered task 02428d64-9511-4623-a243-f562a264b69e, total tasks: 9
[210.780] Executor: Adding task 02428d64-9511-4623-a243-f562a264b69e to server 0 (has 3 workers)
[210.780] Server 0: Adding new task
[210.781] Creating new TaskEntry with ID: 2122747d-c2a8-4c27-bd08-a07a608b768f
[210.781] TaskQueue: Enqueueing task 2122747d-c2a8-4c27-bd08-a07a608b768f
[210.781] TaskQueue: Task 2122747d-c2a8-4c27-bd08-a07a608b768f added to pending queue
[210.782] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[210.782] Server 0: Task queued
[210.782] Task 02428d64-9511-4623-a243-f562a264b69e assigned to server 0
[210.782] UMCG syscall result: 0
[210.782] Server 0: Processing event 1 for worker 6
[210.783] Server 0: Worker 6 blocked
[210.783] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(c3b1dd8c-bc64-48f9-9420-bfe34d1b2258))
[210.784] Server 0: Attempting to schedule tasks...
[210.784] Server 0: Looking for waiting workers among 3 workers
[210.784] Server 0: No waiting workers found
[210.784] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[210.785] Server 0: Attempting to schedule tasks...
[210.785] Server 0: Looking for waiting workers among 3 workers
[210.785] Server 0: No waiting workers found
[210.785] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[210.786] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.786] UMCG syscall result: 0
[210.786] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[210.786] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[210.787] Server 0: Processing event 2 for worker 5
[210.787] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.787] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[210.788] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.788] Server 0: Worker 5 unblocking
[210.788] WorkerPool: Updating worker 1 status from Blocked to Running
[210.788] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[210.789] Server 0: Context switching to worker 5
[210.789] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[210.789] TaskHandle: Task submitted successfully
[210.790] !!!! Initial task 0: COMPLETED !!!!
[210.792] Completed task 24cec5ef-0c32-4f5d-a508-c72f77926d6f, total completed: 1/9
[210.792] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[210.793] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[210.793] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.794] UMCG syscall result: 0
[210.794] Server 0: Processing event 3 for worker 5
[210.795] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[210.795] Server 0: Got explicit WAIT from worker 5
[210.796] Server 0: Worker 5 current status: Running
[210.796] WorkerPool: Updating worker 1 status from Running to Waiting
[210.796] Server 0: Attempting to schedule tasks...
[210.796] Server 0: Looking for waiting workers among 3 workers
[210.796] Server 0: Found waiting worker 1 with status Waiting
[210.797] TaskQueue: Attempting to get next task
[210.797] TaskQueue: Retrieved task db803747-541a-44d8-9d51-1f5bbe3a1a5d
[210.797] Server 0: Assigning task to worker 1
[210.797] WorkerPool: Updating worker 1 status from Waiting to Running
[210.798] TaskQueue: Marking task db803747-541a-44d8-9d51-1f5bbe3a1a5d as in progress with worker 5
[210.798] Worker 1: Starting task assignment
[210.798] Worker 1: Task sent successfully
[210.799] Server 0: Context switching to worker 5
[210.799] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[210.799] UMCG syscall result: 0
[210.800] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[210.801] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[210.801] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[210.802] !!!! Initial task 3: STARTING task !!!!
[210.802] !!!! Initial task 3: ABOUT TO SLEEP !!!!
[210.802] UMCG syscall result: 0
[210.802] Server 0: Processing event 1 for worker 5
[210.803] Server 0: Worker 5 blocked
[210.803] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(db803747-541a-44d8-9d51-1f5bbe3a1a5d))
[210.803] Server 0: Successfully switched to worker 5
[210.804] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[210.804] Server 0: Processing event 2 for worker 4
[210.804] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.805] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[210.805] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.805] Server 0: Worker 4 unblocking
[210.805] WorkerPool: Updating worker 0 status from Blocked to Running
[210.805] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[210.805] Server 0: Context switching to worker 4
[210.806] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[210.806] TaskHandle: Task submitted successfully
[210.806] !!!! Initial task 1: COMPLETED !!!!
[210.807] Completed task c9817158-04f0-49df-9c43-6e75bb379272, total completed: 2/9
[210.808] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[210.809] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[210.809] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.809] UMCG syscall result: 0
[210.810] Server 0: Processing event 3 for worker 4
[210.810] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[210.810] Server 0: Got explicit WAIT from worker 4
[210.810] Server 0: Worker 4 current status: Running
[210.811] WorkerPool: Updating worker 0 status from Running to Waiting
[210.811] Server 0: Attempting to schedule tasks...
[210.811] Server 0: Looking for waiting workers among 3 workers
[210.811] Server 0: Found waiting worker 0 with status Waiting
[210.811] TaskQueue: Attempting to get next task
[210.812] TaskQueue: Retrieved task b3480109-806d-4472-b766-8acbf6008f23
[210.812] Server 0: Assigning task to worker 0
[210.812] WorkerPool: Updating worker 0 status from Waiting to Running
[210.812] TaskQueue: Marking task b3480109-806d-4472-b766-8acbf6008f23 as in progress with worker 4
[210.812] Worker 0: Starting task assignment
[210.813] Worker 0: Task sent successfully
[210.813] Server 0: Context switching to worker 4
[210.813] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[210.813] UMCG syscall result: 0
[210.814] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[210.814] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[210.815] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[210.815] !!!! Initial task 4: STARTING task !!!!
[210.815] !!!! Initial task 4: ABOUT TO SLEEP !!!!
[210.815] UMCG syscall result: 0
[210.815] Server 0: Processing event 1 for worker 4
[210.815] Server 0: Worker 4 blocked
[210.816] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(b3480109-806d-4472-b766-8acbf6008f23))
[210.816] Server 0: Successfully switched to worker 4
[210.816] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[210.816] Server 0: Attempting to schedule tasks...
[210.817] Server 0: Looking for waiting workers among 3 workers
[210.817] Server 0: No waiting workers found
[210.817] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[210.817] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.817] UMCG syscall result: 0
[210.817] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[210.818] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[210.818] Server 0: Processing event 2 for worker 6
[210.818] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[210.818] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[210.818] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[210.819] Server 0: Worker 6 unblocking
[210.819] WorkerPool: Updating worker 2 status from Blocked to Running
[210.819] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[210.819] Server 0: Context switching to worker 6
[210.819] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[210.820] TaskHandle: Task submitted successfully
[210.820] !!!! Initial task 2: COMPLETED !!!!
[210.820] Completed task 93a0e583-a197-49d9-a0b4-b8c946f520f1, total completed: 3/9
[210.820] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[210.821] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[210.821] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[210.821] UMCG syscall result: 0
[210.821] Server 0: Processing event 3 for worker 6
[210.821] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[210.822] Server 0: Got explicit WAIT from worker 6
[210.822] Server 0: Worker 6 current status: Running
[210.822] WorkerPool: Updating worker 2 status from Running to Waiting
[210.822] Server 0: Attempting to schedule tasks...
[210.823] Server 0: Looking for waiting workers among 3 workers
[210.824] Server 0: Found waiting worker 2 with status Waiting
[210.824] TaskQueue: Attempting to get next task
[210.825] TaskQueue: Retrieved task 07f09dd3-6985-41b0-96ec-f492ded43f61
[210.825] Server 0: Assigning task to worker 2
[210.825] WorkerPool: Updating worker 2 status from Waiting to Running
[210.825] TaskQueue: Marking task 07f09dd3-6985-41b0-96ec-f492ded43f61 as in progress with worker 6
[210.825] Worker 2: Starting task assignment
[210.826] Worker 2: Task sent successfully
[210.826] Server 0: Context switching to worker 6
[210.826] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[210.826] UMCG syscall result: 0
[210.827] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[210.827] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[210.827] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[210.828] !!!! Initial task 5: STARTING task !!!!
[210.828] !!!! Initial task 5: ABOUT TO SLEEP !!!!
[210.828] UMCG syscall result: 0
[210.828] Server 0: Processing event 1 for worker 6
[210.828] Server 0: Worker 6 blocked
[210.829] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(07f09dd3-6985-41b0-96ec-f492ded43f61))
[210.829] Server 0: Successfully switched to worker 6
[210.829] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[210.830] Server 0: Attempting to schedule tasks...
[210.830] Server 0: Looking for waiting workers among 3 workers
[210.830] Server 0: No waiting workers found
[210.830] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[210.830] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.809] UMCG syscall result: 0
[212.810] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[212.811] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[212.812] Server 0: Processing event 2 for worker 5
[212.813] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.814] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[212.814] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.816] Server 0: Worker 5 unblocking
[212.817] WorkerPool: Updating worker 1 status from Blocked to Running
[212.817] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[212.818] Server 0: Context switching to worker 5
[212.818] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[212.819] !!!! Initial task 3: WOKE UP FROM SLEEP !!!!
[212.819] !!!! Initial task 3: PREPARING to spawn child task !!!!
[212.820] TaskHandle: Submitting new task
[212.820] Executor: Submitting new task
[212.822] Registered task 1d41992d-1236-4e6f-9853-738d20907edd, total tasks: 10
[212.823] Executor: Adding task 1d41992d-1236-4e6f-9853-738d20907edd to server 0 (has 3 workers)
[212.823] Server 0: Adding new task
[212.824] Creating new TaskEntry with ID: 29c809d8-c68e-4196-a897-f55810a58688
[212.824] TaskQueue: Enqueueing task 29c809d8-c68e-4196-a897-f55810a58688
[212.824] TaskQueue: Task 29c809d8-c68e-4196-a897-f55810a58688 added to pending queue
[212.825] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[212.826] Server 0: Task queued
[212.826] Task 1d41992d-1236-4e6f-9853-738d20907edd assigned to server 0
[212.827] UMCG syscall result: 0
[212.827] Server 0: Processing event 1 for worker 5
[212.827] Server 0: Worker 5 blocked
[212.828] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(db803747-541a-44d8-9d51-1f5bbe3a1a5d))
[212.829] Server 0: Attempting to schedule tasks...
[212.829] Server 0: Looking for waiting workers among 3 workers
[212.829] Server 0: No waiting workers found
[212.830] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[212.830] Server 0: Attempting to schedule tasks...
[212.831] Server 0: Looking for waiting workers among 3 workers
[212.831] Server 0: No waiting workers found
[212.832] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[212.832] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.833] UMCG syscall result: 0
[212.833] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[212.833] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[212.834] Server 0: Processing event 2 for worker 4
[212.834] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.834] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[212.834] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.835] Server 0: Worker 4 unblocking
[212.835] WorkerPool: Updating worker 0 status from Blocked to Running
[212.835] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[212.836] Server 0: Context switching to worker 4
[212.836] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[212.836] !!!! Initial task 4: WOKE UP FROM SLEEP !!!!
[212.837] !!!! Initial task 4: PREPARING to spawn child task !!!!
[212.837] TaskHandle: Submitting new task
[212.837] Executor: Submitting new task
[212.838] Registered task e3a066e0-6007-40bd-aef2-bda39bd28722, total tasks: 11
[212.838] Executor: Adding task e3a066e0-6007-40bd-aef2-bda39bd28722 to server 0 (has 3 workers)
[212.839] Server 0: Adding new task
[212.839] Creating new TaskEntry with ID: 32fa9823-2ae9-4045-b539-8b4a0237784d
[212.839] TaskQueue: Enqueueing task 32fa9823-2ae9-4045-b539-8b4a0237784d
[212.840] TaskQueue: Task 32fa9823-2ae9-4045-b539-8b4a0237784d added to pending queue
[212.840] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[212.840] Server 0: Task queued
[212.840] Task e3a066e0-6007-40bd-aef2-bda39bd28722 assigned to server 0
[212.841] UMCG syscall result: 0
[212.841] Server 0: Processing event 1 for worker 4
[212.842] Server 0: Worker 4 blocked
[212.842] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(b3480109-806d-4472-b766-8acbf6008f23))
[212.842] Server 0: Attempting to schedule tasks...
[212.843] Server 0: Looking for waiting workers among 3 workers
[212.843] Server 0: No waiting workers found
[212.843] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[212.843] Server 0: Processing event 2 for worker 6
[212.844] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.844] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[212.844] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.845] Server 0: Worker 6 unblocking
[212.845] WorkerPool: Updating worker 2 status from Blocked to Running
[212.845] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[212.845] Server 0: Context switching to worker 6
[212.846] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[212.846] !!!! Initial task 5: WOKE UP FROM SLEEP !!!!
[212.846] !!!! Initial task 5: PREPARING to spawn child task !!!!
[212.846] TaskHandle: Submitting new task
[212.847] Executor: Submitting new task
[212.849] Registered task c3f4447d-e5ef-45e2-80d7-f6590506e021, total tasks: 12
[212.850] Executor: Adding task c3f4447d-e5ef-45e2-80d7-f6590506e021 to server 0 (has 3 workers)
[212.850] Server 0: Adding new task
[212.850] Creating new TaskEntry with ID: a4d4b552-ad78-426d-8663-e6e9d5b8c3d5
[212.851] TaskQueue: Enqueueing task a4d4b552-ad78-426d-8663-e6e9d5b8c3d5
[212.851] TaskQueue: Task a4d4b552-ad78-426d-8663-e6e9d5b8c3d5 added to pending queue
[212.851] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[212.852] Server 0: Task queued
[212.852] Task c3f4447d-e5ef-45e2-80d7-f6590506e021 assigned to server 0
[212.852] UMCG syscall result: 0
[212.852] Server 0: Processing event 1 for worker 6
[212.853] Server 0: Worker 6 blocked
[212.853] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(07f09dd3-6985-41b0-96ec-f492ded43f61))
[212.853] Server 0: Attempting to schedule tasks...
[212.854] Server 0: Looking for waiting workers among 3 workers
[212.854] Server 0: No waiting workers found
[212.854] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[212.854] Server 0: Attempting to schedule tasks...
[212.855] Server 0: Looking for waiting workers among 3 workers
[212.855] Server 0: No waiting workers found
[212.855] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[212.855] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.856] UMCG syscall result: 0
[212.856] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[212.856] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[212.856] Server 0: Processing event 2 for worker 5
[212.856] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.857] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[212.857] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.857] Server 0: Worker 5 unblocking
[212.857] WorkerPool: Updating worker 1 status from Blocked to Running
[212.858] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[212.858] Server 0: Context switching to worker 5
[212.858] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[212.859] TaskHandle: Task submitted successfully
[212.859] !!!! Initial task 3: COMPLETED !!!!
[212.859] Completed task de0699ef-61dd-4462-a6cc-73afd8b4c5ca, total completed: 4/12
[212.860] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[212.860] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[212.860] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.860] UMCG syscall result: 0
[212.861] Server 0: Processing event 3 for worker 5
[212.861] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[212.861] Server 0: Got explicit WAIT from worker 5
[212.861] Server 0: Worker 5 current status: Running
[212.862] WorkerPool: Updating worker 1 status from Running to Waiting
[212.862] Server 0: Attempting to schedule tasks...
[212.862] Server 0: Looking for waiting workers among 3 workers
[212.862] Server 0: Found waiting worker 1 with status Waiting
[212.863] TaskQueue: Attempting to get next task
[212.863] TaskQueue: Retrieved task 6d67623e-c1a1-47ed-abe4-7a428e9b7492
[212.863] Server 0: Assigning task to worker 1
[212.864] WorkerPool: Updating worker 1 status from Waiting to Running
[212.864] TaskQueue: Marking task 6d67623e-c1a1-47ed-abe4-7a428e9b7492 as in progress with worker 5
[212.864] Worker 1: Starting task assignment
[212.865] Worker 1: Task sent successfully
[212.865] Server 0: Context switching to worker 5
[212.865] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[212.865] UMCG syscall result: 0
[212.866] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[212.866] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[212.867] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[212.867] !!!! Child task of initial task 0: STARTING work !!!!
[212.868] UMCG syscall result: 0
[212.868] Server 0: Processing event 1 for worker 5
[212.868] Server 0: Worker 5 blocked
[212.868] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(6d67623e-c1a1-47ed-abe4-7a428e9b7492))
[212.868] Server 0: Successfully switched to worker 5
[212.869] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[212.869] Server 0: Processing event 2 for worker 4
[212.869] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.869] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[212.869] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.870] Server 0: Worker 4 unblocking
[212.870] WorkerPool: Updating worker 0 status from Blocked to Running
[212.870] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[212.870] Server 0: Context switching to worker 4
[212.870] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[212.871] TaskHandle: Task submitted successfully
[212.871] !!!! Initial task 4: COMPLETED !!!!
[212.871] Completed task 0b6825d4-de03-4533-a0a8-132b554f5581, total completed: 5/12
[212.872] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[212.872] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[212.872] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.872] UMCG syscall result: 0
[212.872] Server 0: Processing event 3 for worker 4
[212.873] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[212.873] Server 0: Got explicit WAIT from worker 4
[212.873] Server 0: Worker 4 current status: Running
[212.874] WorkerPool: Updating worker 0 status from Running to Waiting
[212.874] Server 0: Attempting to schedule tasks...
[212.874] Server 0: Looking for waiting workers among 3 workers
[212.874] Server 0: Found waiting worker 0 with status Waiting
[212.874] TaskQueue: Attempting to get next task
[212.875] TaskQueue: Retrieved task 22e58ebb-d9d8-4a7a-9106-38ffe2daba16
[212.875] Server 0: Assigning task to worker 0
[212.875] WorkerPool: Updating worker 0 status from Waiting to Running
[212.875] TaskQueue: Marking task 22e58ebb-d9d8-4a7a-9106-38ffe2daba16 as in progress with worker 4
[212.876] Worker 0: Starting task assignment
[212.876] Worker 0: Task sent successfully
[212.876] Server 0: Context switching to worker 4
[212.876] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[212.877] UMCG syscall result: 0
[212.877] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[212.877] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[212.878] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[212.878] !!!! Child task of initial task 1: STARTING work !!!!
[212.878] UMCG syscall result: 0
[212.878] Server 0: Processing event 1 for worker 4
[212.878] Server 0: Worker 4 blocked
[212.879] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(22e58ebb-d9d8-4a7a-9106-38ffe2daba16))
[212.879] Server 0: Successfully switched to worker 4
[212.879] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[212.879] Server 0: Attempting to schedule tasks...
[212.880] Server 0: Looking for waiting workers among 3 workers
[212.880] Server 0: No waiting workers found
[212.880] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[212.880] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.880] UMCG syscall result: 0
[212.880] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[212.881] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[212.881] Server 0: Processing event 2 for worker 6
[212.881] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[212.881] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[212.881] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[212.882] Server 0: Worker 6 unblocking
[212.882] WorkerPool: Updating worker 2 status from Blocked to Running
[212.882] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[212.883] Server 0: Context switching to worker 6
[212.883] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[212.883] TaskHandle: Task submitted successfully
[212.883] !!!! Initial task 5: COMPLETED !!!!
[212.883] Completed task b6c72c64-a201-48e6-bb8e-399a9d2bbee1, total completed: 6/12
[212.884] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[212.884] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[212.884] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[212.885] UMCG syscall result: 0
[212.885] Server 0: Processing event 3 for worker 6
[212.885] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[212.885] Server 0: Got explicit WAIT from worker 6
[212.885] Server 0: Worker 6 current status: Running
[212.885] WorkerPool: Updating worker 2 status from Running to Waiting
[212.886] Server 0: Attempting to schedule tasks...
[212.886] Server 0: Looking for waiting workers among 3 workers
[212.886] Server 0: Found waiting worker 2 with status Waiting
[212.886] TaskQueue: Attempting to get next task
[212.887] TaskQueue: Retrieved task 2122747d-c2a8-4c27-bd08-a07a608b768f
[212.887] Server 0: Assigning task to worker 2
[212.887] WorkerPool: Updating worker 2 status from Waiting to Running
[212.887] TaskQueue: Marking task 2122747d-c2a8-4c27-bd08-a07a608b768f as in progress with worker 6
[212.888] Worker 2: Starting task assignment
[212.888] Worker 2: Task sent successfully
[212.888] Server 0: Context switching to worker 6
[212.888] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[212.888] UMCG syscall result: 0
[212.889] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[212.889] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[212.889] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[212.890] !!!! Child task of initial task 2: STARTING work !!!!
[212.890] UMCG syscall result: 0
[212.890] Server 0: Processing event 1 for worker 6
[212.890] Server 0: Worker 6 blocked
[212.891] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(2122747d-c2a8-4c27-bd08-a07a608b768f))
[212.891] Server 0: Successfully switched to worker 6
[212.891] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[212.891] Server 0: Attempting to schedule tasks...
[212.891] Server 0: Looking for waiting workers among 3 workers
[212.891] Server 0: No waiting workers found
[212.892] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[212.892] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.766] Progress: 6/12 tasks completed
[213.874] Progress: 6/12 tasks completed
[213.874] UMCG syscall result: 0
[213.875] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[213.876] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[213.876] Server 0: Processing event 2 for worker 5
[213.877] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[213.877] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[213.878] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[213.878] Server 0: Worker 5 unblocking
[213.879] WorkerPool: Updating worker 1 status from Blocked to Running
[213.879] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[213.879] Server 0: Context switching to worker 5
[213.879] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[213.879] !!!! Child task of initial task 0: COMPLETED !!!!
[213.880] Completed task bcb73d2a-bddc-43a2-9404-e8c711673b5c, total completed: 7/12
[213.881] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[213.882] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[213.882] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.882] UMCG syscall result: 0
[213.882] Server 0: Processing event 3 for worker 5
[213.883] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[213.883] Server 0: Got explicit WAIT from worker 5
[213.883] Server 0: Worker 5 current status: Running
[213.884] WorkerPool: Updating worker 1 status from Running to Waiting
[213.884] Server 0: Attempting to schedule tasks...
[213.885] Server 0: Looking for waiting workers among 3 workers
[213.885] Server 0: Found waiting worker 1 with status Waiting
[213.885] TaskQueue: Attempting to get next task
[213.885] TaskQueue: Retrieved task 29c809d8-c68e-4196-a897-f55810a58688
[213.886] Server 0: Assigning task to worker 1
[213.886] WorkerPool: Updating worker 1 status from Waiting to Running
[213.886] TaskQueue: Marking task 29c809d8-c68e-4196-a897-f55810a58688 as in progress with worker 5
[213.887] Worker 1: Starting task assignment
[213.887] Worker 1: Task sent successfully
[213.887] Server 0: Context switching to worker 5
[213.887] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[213.888] UMCG syscall result: 0
[213.888] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[213.889] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[213.889] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[213.889] !!!! Child task of initial task 3: STARTING work !!!!
[213.890] UMCG syscall result: 0
[213.890] Server 0: Processing event 1 for worker 5
[213.890] Server 0: Worker 5 blocked
[213.891] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(29c809d8-c68e-4196-a897-f55810a58688))
[213.892] Server 0: Successfully switched to worker 5
[213.892] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[213.892] Server 0: Attempting to schedule tasks...
[213.892] Server 0: Looking for waiting workers among 3 workers
[213.893] Server 0: No waiting workers found
[213.893] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[213.893] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.894] UMCG syscall result: 0
[213.894] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[213.894] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[213.894] Server 0: Processing event 2 for worker 4
[213.895] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[213.895] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[213.895] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[213.895] Server 0: Worker 4 unblocking
[213.896] WorkerPool: Updating worker 0 status from Blocked to Running
[213.896] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[213.896] Server 0: Context switching to worker 4
[213.897] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[213.897] !!!! Child task of initial task 1: COMPLETED !!!!
[213.897] Completed task 65a2e234-4ab4-4cdc-9280-2d3e94f8c1d0, total completed: 8/12
[213.898] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[213.898] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[213.899] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.899] UMCG syscall result: 0
[213.899] Server 0: Processing event 3 for worker 4
[213.899] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[213.899] Server 0: Got explicit WAIT from worker 4
[213.900] Server 0: Worker 4 current status: Running
[213.900] WorkerPool: Updating worker 0 status from Running to Waiting
[213.900] Server 0: Attempting to schedule tasks...
[213.901] Server 0: Looking for waiting workers among 3 workers
[213.901] Server 0: Found waiting worker 0 with status Waiting
[213.901] TaskQueue: Attempting to get next task
[213.901] TaskQueue: Retrieved task 32fa9823-2ae9-4045-b539-8b4a0237784d
[213.902] Server 0: Assigning task to worker 0
[213.902] WorkerPool: Updating worker 0 status from Waiting to Running
[213.903] TaskQueue: Marking task 32fa9823-2ae9-4045-b539-8b4a0237784d as in progress with worker 4
[213.903] Worker 0: Starting task assignment
[213.903] Worker 0: Task sent successfully
[213.903] Server 0: Context switching to worker 4
[213.903] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[213.904] UMCG syscall result: 0
[213.904] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[213.904] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[213.905] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[213.905] !!!! Child task of initial task 4: STARTING work !!!!
[213.905] UMCG syscall result: 0
[213.906] Server 0: Processing event 1 for worker 4
[213.906] Server 0: Worker 4 blocked
[213.906] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(32fa9823-2ae9-4045-b539-8b4a0237784d))
[213.906] Server 0: Successfully switched to worker 4
[213.907] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[213.907] Server 0: Processing event 2 for worker 6
[213.907] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[213.907] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[213.908] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[213.908] Server 0: Worker 6 unblocking
[213.908] WorkerPool: Updating worker 2 status from Blocked to Running
[213.908] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[213.909] Server 0: Context switching to worker 6
[213.909] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[213.909] !!!! Child task of initial task 2: COMPLETED !!!!
[213.909] Completed task 02428d64-9511-4623-a243-f562a264b69e, total completed: 9/12
[213.910] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[213.910] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[213.910] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.911] UMCG syscall result: 0
[213.911] Server 0: Processing event 3 for worker 6
[213.911] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[213.911] Server 0: Got explicit WAIT from worker 6
[213.912] Server 0: Worker 6 current status: Running
[213.912] WorkerPool: Updating worker 2 status from Running to Waiting
[213.912] Server 0: Attempting to schedule tasks...
[213.912] Server 0: Looking for waiting workers among 3 workers
[213.912] Server 0: Found waiting worker 2 with status Waiting
[213.913] TaskQueue: Attempting to get next task
[213.913] TaskQueue: Retrieved task a4d4b552-ad78-426d-8663-e6e9d5b8c3d5
[213.913] Server 0: Assigning task to worker 2
[213.913] WorkerPool: Updating worker 2 status from Waiting to Running
[213.913] TaskQueue: Marking task a4d4b552-ad78-426d-8663-e6e9d5b8c3d5 as in progress with worker 6
[213.914] Worker 2: Starting task assignment
[213.914] Worker 2: Task sent successfully
[213.914] Server 0: Context switching to worker 6
[213.915] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[213.915] UMCG syscall result: 0
[213.915] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[213.916] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[213.916] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[213.916] !!!! Child task of initial task 5: STARTING work !!!!
[213.917] UMCG syscall result: 0
[213.917] Server 0: Processing event 1 for worker 6
[213.917] Server 0: Worker 6 blocked
[213.917] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(a4d4b552-ad78-426d-8663-e6e9d5b8c3d5))
[213.918] Server 0: Successfully switched to worker 6
[213.918] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[213.918] Server 0: Attempting to schedule tasks...
[213.918] Server 0: Looking for waiting workers among 3 workers
[213.918] Server 0: No waiting workers found
[213.919] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[213.919] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[213.979] Progress: 9/12 tasks completed
[214.089] Progress: 9/12 tasks completed
[214.202] Progress: 9/12 tasks completed
[214.306] Progress: 9/12 tasks completed
[214.416] Progress: 9/12 tasks completed
[214.525] Progress: 9/12 tasks completed
[214.629] Progress: 9/12 tasks completed
[214.897] UMCG syscall result: 0
[214.899] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[214.900] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[214.902] Server 0: Processing event 2 for worker 5
[214.902] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[214.903] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[214.904] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[214.905] Server 0: Worker 5 unblocking
[214.906] WorkerPool: Updating worker 1 status from Blocked to Running
[214.907] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[214.907] Server 0: Context switching to worker 5
[214.907] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[214.908] !!!! Child task of initial task 3: COMPLETED !!!!
[214.908] Completed task 1d41992d-1236-4e6f-9853-738d20907edd, total completed: 10/12
[214.909] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[214.909] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[214.910] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[214.910] UMCG syscall result: 0
[214.910] Server 0: Processing event 3 for worker 5
[214.911] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[214.911] Server 0: Got explicit WAIT from worker 5
[214.912] Server 0: Worker 5 current status: Running
[214.912] WorkerPool: Updating worker 1 status from Running to Waiting
[214.912] Server 0: Attempting to schedule tasks...
[214.913] Server 0: Looking for waiting workers among 3 workers
[214.913] Server 0: Found waiting worker 1 with status Waiting
[214.913] TaskQueue: Attempting to get next task
[214.914] TaskQueue: No tasks available
[214.915] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[214.915] Server 0: Attempting to schedule tasks...
[214.915] Server 0: Looking for waiting workers among 3 workers
[214.916] Server 0: Found waiting worker 1 with status Waiting
[214.916] TaskQueue: Attempting to get next task
[214.917] TaskQueue: No tasks available
[214.917] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[214.918] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[214.918] UMCG syscall result: 0
[214.918] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[214.919] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[214.919] Server 0: Processing event 2 for worker 4
[214.920] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[214.920] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[214.920] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[214.921] Server 0: Worker 4 unblocking
[214.921] WorkerPool: Updating worker 0 status from Blocked to Running
[214.921] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[214.922] Server 0: Context switching to worker 4
[214.922] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[214.922] !!!! Child task of initial task 4: COMPLETED !!!!
[214.923] Completed task e3a066e0-6007-40bd-aef2-bda39bd28722, total completed: 11/12
[214.924] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[214.925] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[214.925] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[214.926] UMCG syscall result: 0
[214.926] Server 0: Processing event 3 for worker 4
[214.926] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[214.927] Server 0: Got explicit WAIT from worker 4
[214.927] Server 0: Worker 4 current status: Running
[214.927] WorkerPool: Updating worker 0 status from Running to Waiting
[214.927] Server 0: Attempting to schedule tasks...
[214.928] Server 0: Looking for waiting workers among 3 workers
[214.928] Server 0: Found waiting worker 1 with status Waiting
[214.928] TaskQueue: Attempting to get next task
[214.928] TaskQueue: No tasks available
[214.929] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[214.929] Server 0: Processing event 2 for worker 6
[214.929] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[214.930] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[214.930] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[214.930] Server 0: Worker 6 unblocking
[214.931] WorkerPool: Updating worker 2 status from Blocked to Running
[214.931] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[214.931] Server 0: Context switching to worker 6
[214.931] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[214.932] !!!! Child task of initial task 5: COMPLETED !!!!
[214.933] Completed task c3f4447d-e5ef-45e2-80d7-f6590506e021, total completed: 12/12
[214.933] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[214.934] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[214.934] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[214.935] UMCG syscall result: 0
[214.935] Server 0: Processing event 3 for worker 6
[214.935] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[214.935] Server 0: Got explicit WAIT from worker 6
[214.936] Server 0: Worker 6 current status: Running
[214.936] WorkerPool: Updating worker 2 status from Running to Waiting
[214.936] Server 0: Attempting to schedule tasks...
[214.937] Server 0: Looking for waiting workers among 3 workers
[214.937] Server 0: Found waiting worker 1 with status Waiting
[214.937] TaskQueue: Attempting to get next task
[214.937] TaskQueue: No tasks available
[214.937] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[214.938] Server 0: Attempting to schedule tasks...
[214.937] All tasks completed successfully (12/12)
[214.938] Server 0: Looking for waiting workers among 3 workers
[214.939] Server 0: Found waiting worker 1 with status Waiting
[214.939] TaskQueue: Attempting to get next task
[214.939] TaskQueue: No tasks available
[214.938] Initiating executor shutdown...
[214.940] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[214.940] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[214.940] Server 0: Adding new task
[214.941] Server 0: Received shutdown signal
Running run_multi_server_demo
➜  /app git:(master) ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[257.459] Creating new Executor
[257.475] Creating Server 0 on CPU 0
[257.482] Creating Server 0
[257.484] Creating new TaskQueue
[257.486] Creating WorkerPool with capacity for 3 workers
[257.488] Starting executor...
[257.489] Executor: Starting servers
[257.509] Executor: All servers started
[257.521] Successfully set CPU affinity to 0
[257.533] Server 0: Starting up
[257.535] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[257.541] UMCG syscall result: 0
[257.542] Server 0: UMCG registration complete
[257.543] Server 0: Initializing workers
[257.544] Server 0: Initializing worker 0
[257.546] Starting WorkerThread 0 for server 0
[257.558] Successfully set CPU affinity to 0
[257.559] Worker 0: Initialized with tid 4
[257.561] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[257.562] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[257.572] Submitting initial tasks...
[257.573] !!!! Submitting initial task 0 !!!!
[257.573] Executor: Submitting new task
[257.588] WorkerThread 0 started with tid 4 for server 0
[257.589] Creating new Worker 0 for server 0
[257.587] Registered task 0b23b6ed-960d-4d1d-a572-1509043fb970, total tasks: 1
[257.589] WorkerPool: Adding worker 0 with tid 4
[257.597] WorkerPool: Updating worker 0 status from Initializing to Registering
[257.596] Executor: Adding task 0b23b6ed-960d-4d1d-a572-1509043fb970 to server 0 (has 1 workers)
[257.598] Server 0: Adding new task
[257.599] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[257.602] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[257.605] UMCG syscall result: 0
[257.605] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[257.606] Worker 0: Registering worker event 130
[257.606] WorkerPool: Updating worker 0 status from Registering to Waiting
[257.602] Creating new TaskEntry with ID: 7db9b8c6-a47e-4807-89f6-15e468dd5ba9
[257.607] Server 0: Worker 0 initialized successfully
[257.609] Server 0: Initializing worker 1
[257.610] Starting WorkerThread 1 for server 0
[257.609] TaskQueue: Enqueueing task 7db9b8c6-a47e-4807-89f6-15e468dd5ba9
[257.611] TaskQueue: Task 7db9b8c6-a47e-4807-89f6-15e468dd5ba9 added to pending queue
[257.616] Successfully set CPU affinity to 0
[257.613] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[257.616] Worker 1: Initialized with tid 5
[257.617] Server 0: Task queued
[257.619] Task 0b23b6ed-960d-4d1d-a572-1509043fb970 assigned to server 0
[257.620] !!!! Submitting initial task 1 !!!!
[257.622] Executor: Submitting new task
[257.621] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[257.625] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[257.624] WorkerThread 1 started with tid 5 for server 0
[257.626] Registered task 2c67a5f7-9fe3-4102-825d-1ab16b319c4e, total tasks: 2
[257.626] Creating new Worker 1 for server 0
[257.628] WorkerPool: Adding worker 1 with tid 5
[257.629] WorkerPool: Updating worker 1 status from Initializing to Registering
[257.628] Executor: Adding task 2c67a5f7-9fe3-4102-825d-1ab16b319c4e to server 0 (has 1 workers)
[257.630] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[257.630] Server 0: Adding new task
[257.632] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[257.634] UMCG syscall result: 0
[257.635] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[257.635] Worker 0: Registering worker event 162
[257.635] WorkerPool: Updating worker 1 status from Registering to Waiting
[257.635] Creating new TaskEntry with ID: 1c3bb702-16da-4e65-bac8-fdb50cb41514
[257.636] Server 0: Worker 1 initialized successfully
[257.637] Server 0: Initializing worker 2
[257.637] Starting WorkerThread 2 for server 0
[257.636] TaskQueue: Enqueueing task 1c3bb702-16da-4e65-bac8-fdb50cb41514
[257.638] Successfully set CPU affinity to 0
[257.639] TaskQueue: Task 1c3bb702-16da-4e65-bac8-fdb50cb41514 added to pending queue
[257.639] Worker 2: Initialized with tid 6
[257.640] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[257.641] Server 0: Task queued
[257.641] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[257.642] Task 2c67a5f7-9fe3-4102-825d-1ab16b319c4e assigned to server 0
[257.643] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[257.642] WorkerThread 2 started with tid 6 for server 0
[257.645] !!!! Submitting initial task 2 !!!!
[257.646] Executor: Submitting new task
[257.647] Creating new Worker 2 for server 0
[257.648] WorkerPool: Adding worker 2 with tid 6
[257.648] WorkerPool: Updating worker 2 status from Initializing to Registering
[257.647] Registered task 1b0ae54b-4ded-4a1e-8337-68ae06eedeeb, total tasks: 3
[257.649] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[257.650] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[257.649] Executor: Adding task 1b0ae54b-4ded-4a1e-8337-68ae06eedeeb to server 0 (has 3 workers)
[257.651] Server 0: Adding new task
[257.651] UMCG syscall result: 0
[257.652] Creating new TaskEntry with ID: 132d7bcd-40b8-4e5e-bcd7-da98a19cdeed
[257.652] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[257.652] Worker 0: Registering worker event 194
[257.652] TaskQueue: Enqueueing task 132d7bcd-40b8-4e5e-bcd7-da98a19cdeed
[257.653] TaskQueue: Task 132d7bcd-40b8-4e5e-bcd7-da98a19cdeed added to pending queue
[257.653] WorkerPool: Updating worker 2 status from Registering to Waiting
[257.654] Server 0: Worker 2 initialized successfully
[257.654] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[257.654] Server 0: Task queued
[257.654] Server 0: All workers initialized
[257.655] Task 1b0ae54b-4ded-4a1e-8337-68ae06eedeeb assigned to server 0
[257.655] !!!! Submitting initial task 3 !!!!
[257.655] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[257.656] Executor: Submitting new task
[257.658] Server 0: Attempting to schedule tasks...
[257.661] Server 0: Looking for waiting workers among 3 workers
[257.661] Server 0: Found waiting worker 1 with status Waiting
[257.662] TaskQueue: Attempting to get next task
[257.661] Registered task 0940a459-eef8-4df1-a286-c339ccd23efd, total tasks: 4
[257.663] TaskQueue: Retrieved task 7db9b8c6-a47e-4807-89f6-15e468dd5ba9
[257.663] Executor: Adding task 0940a459-eef8-4df1-a286-c339ccd23efd to server 0 (has 3 workers)
[257.664] Server 0: Adding new task
[257.664] Creating new TaskEntry with ID: c6ec4050-ff5c-444d-b25b-7b087f00b3a9
[257.665] Server 0: Assigning task to worker 1
[257.665] WorkerPool: Updating worker 1 status from Waiting to Running
[257.666] TaskQueue: Marking task 7db9b8c6-a47e-4807-89f6-15e468dd5ba9 as in progress with worker 5
[257.667] TaskQueue: Enqueueing task c6ec4050-ff5c-444d-b25b-7b087f00b3a9
[257.667] TaskQueue: Task c6ec4050-ff5c-444d-b25b-7b087f00b3a9 added to pending queue
[257.667] Worker 1: Starting task assignment
[257.668] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[257.671] Worker 1: Task sent successfully
[257.670] Server 0: Task queued
[257.671] Server 0: Context switching to worker 5
[257.672] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[257.672] Task 0940a459-eef8-4df1-a286-c339ccd23efd assigned to server 0
[257.676] !!!! Submitting initial task 4 !!!!
[257.676] Executor: Submitting new task
[257.675] UMCG syscall result: 0
[257.677] Registered task 51b740a4-09bd-4ac2-b6cd-14defc93faa4, total tasks: 5
[257.677] Worker 1: UMCG registration complete with server 0
[257.678] Executor: Adding task 51b740a4-09bd-4ac2-b6cd-14defc93faa4 to server 0 (has 3 workers)
[257.679] Server 0: Adding new task
[257.679] Creating new TaskEntry with ID: 9af320cd-490d-43a2-8e3a-f4f53b58f37c
[257.680] TaskQueue: Enqueueing task 9af320cd-490d-43a2-8e3a-f4f53b58f37c
[257.680] TaskQueue: Task 9af320cd-490d-43a2-8e3a-f4f53b58f37c added to pending queue
[257.680] Worker 1: Entering task processing loop
[257.681] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[257.681] Server 0: Task queued
[257.682] Task 51b740a4-09bd-4ac2-b6cd-14defc93faa4 assigned to server 0
[257.683] !!!! Submitting initial task 5 !!!!
[257.683] Executor: Submitting new task
[257.683] Registered task 94d03f51-9850-4283-8759-bd5c92c879c8, total tasks: 6
[257.683] Executor: Adding task 94d03f51-9850-4283-8759-bd5c92c879c8 to server 0 (has 3 workers)
[257.684] Server 0: Adding new task
[257.686] Creating new TaskEntry with ID: 3db03fad-1922-440e-b7a6-8bae79aca689
[257.686] TaskQueue: Enqueueing task 3db03fad-1922-440e-b7a6-8bae79aca689
[257.687] TaskQueue: Task 3db03fad-1922-440e-b7a6-8bae79aca689 added to pending queue
[257.688] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[257.688] Server 0: Task queued
[257.688] Task 94d03f51-9850-4283-8759-bd5c92c879c8 assigned to server 0
[257.689] All tasks submitted, waiting for completion...
[257.691] Progress: 0/6 tasks completed
[257.799] Progress: 0/6 tasks completed
[257.906] Progress: 0/6 tasks completed
[258.016] Progress: 0/6 tasks completed
[258.154] Progress: 0/6 tasks completed
[258.259] Progress: 0/6 tasks completed
[258.366] Progress: 0/6 tasks completed
[258.471] Progress: 0/6 tasks completed
[258.581] Progress: 0/6 tasks completed
[258.689] Progress: 0/6 tasks completed
en1: assigned FE80::3426:E4FF:FE0C:9286
[262.796] Progress: 0/6 tasks completed
[262.903] Progress: 0/6 tasks completed
[263.010] Progress: 0/6 tasks completed
[263.118] Progress: 0/6 tasks completed
[263.224] Progress: 0/6 tasks completed
[263.328] Progress: 0/6 tasks completed
[263.431] Progress: 0/6 tasks completed
[263.534] Progress: 0/6 tasks completed
[263.636] Progress: 0/6 tasks completed
[267.731] Progress: 0/6 tasks completed
[267.834] Progress: 0/6 tasks completed
[267.937] Progress: 0/6 tasks completed
[268.043] Progress: 0/6 tasks completed
[268.152] Progress: 0/6 tasks completed
[268.261] Progress: 0/6 tasks completed
[268.366] Progress: 0/6 tasks completed
[268.471] Progress: 0/6 tasks completed
[268.580] Progress: 0/6 tasks completed
[268.684] Progress: 0/6 tasks completed
[272.738] Progress: 0/6 tasks completed
[272.844] Progress: 0/6 tasks completed
[272.947] Progress: 0/6 tasks completed
[273.052] Progress: 0/6 tasks completed
[273.161] Progress: 0/6 tasks completed
[273.265] Progress: 0/6 tasks completed
[273.369] Progress: 0/6 tasks completed
[273.479] Progress: 0/6 tasks completed
[273.589] Progress: 0/6 tasks completed
^Cwait: no child processes
➜  /app git:(master)
 */

// pub fn test_worker_first() -> i32 {
//     println!("Testing worker registration before server...");
//
//     // Start worker thread
//     let worker_handle = thread::spawn(|| {
//         let tid = get_thread_id();
//         println!("Worker: registering with tid {}", tid);
//
//         // Register worker
//         assert_eq!(
//             sys_umcg_ctl(
//                 0,
//                 UmcgCmd::RegisterWorker,
//                 0,
//                 (tid as u64) << UMCG_WORKER_ID_SHIFT,
//                 None,
//                 0
//             ),
//             0
//         );
//         println!("Worker: registered, starting sleep");
//         thread::sleep(Duration::from_secs(2));
//         println!("Worker: sleep complete");
//     });
//
//     // Give worker time to register
//     thread::sleep(Duration::from_millis(100));
//
//     // Start server
//     println!("Starting server...");
//     assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
//
//     // Server event loop
//     for _ in 0..10 {
//         println!("Server: waiting for events...");
//         let mut events = [0u64; 6];
//         let ret = umcg_wait_retry(0, Some(&mut events), 6);
//         assert_eq!(ret, 0);
//
//         // Process each event
//         for &event in events.iter().take_while(|&&e| e != 0) {
//             let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
//             let worker_tid = event >> UMCG_WORKER_ID_SHIFT;
//             println!("Server: got event type {} from worker {}", event_type, worker_tid);
//
//             // If we got a WAKE event, try context switching to the worker
//             if event_type == 2 { // WAKE
//                 println!("Server: context switching to worker {}", worker_tid);
//                 let mut switch_events = [0u64; 6];
//                 let ret = sys_umcg_ctl(
//                     0,
//                     UmcgCmd::CtxSwitch,
//                     worker_tid as i32,
//                     0,
//                     Some(&mut switch_events),
//                     6
//                 );
//                 assert_eq!(ret, 0);
//                 println!("Server: context switch returned");
//
//                 // Process any events from the context switch
//                 for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
//                     let switch_event_type = switch_event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
//                     let switch_worker_tid = switch_event >> UMCG_WORKER_ID_SHIFT;
//                     println!("Server: after switch got event type {} from worker {}",
//                              switch_event_type, switch_worker_tid);
//                 }
//             }
//         }
//     }
//
//     worker_handle.join().unwrap();
//     0
// }
//
// pub fn test_server_first() -> i32 {
//     println!("Testing server registration before worker...");
//
//     // Start server first
//     println!("Starting server...");
//     assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
//
//     // Start worker thread
//     let worker_handle = thread::spawn(|| {
//         let tid = get_thread_id();
//         println!("Worker: registering with tid {}", tid);
//
//         // Register worker
//         assert_eq!(
//             sys_umcg_ctl(
//                 0,
//                 UmcgCmd::RegisterWorker,
//                 0,
//                 (tid as u64) << UMCG_WORKER_ID_SHIFT,
//                 None,
//                 0
//             ),
//             0
//         );
//         println!("Worker: registered, starting sleep");
//         thread::sleep(Duration::from_secs(2));
//         println!("Worker: sleep complete");
//     });
//
//     // Server event loop
//     for _ in 0..10 {
//         println!("Server: waiting for events...");
//         let mut events = [0u64; 6];
//         let ret = umcg_wait_retry(0, Some(&mut events), 6);
//         assert_eq!(ret, 0);
//
//         // Process each event
//         for &event in events.iter().take_while(|&&e| e != 0) {
//             let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
//             let worker_tid = event >> UMCG_WORKER_ID_SHIFT;
//             println!("Server: got event type {} from worker {}", event_type, worker_tid);
//
//             // If we got a WAKE event, try context switching to the worker
//             if event_type == 2 { // WAKE
//                 println!("Server: context switching to worker {}", worker_tid);
//                 let mut switch_events = [0u64; 6];
//                 let ret = sys_umcg_ctl(
//                     0,
//                     UmcgCmd::CtxSwitch,
//                     worker_tid as i32,
//                     0,
//                     Some(&mut switch_events),
//                     6
//                 );
//                 assert_eq!(ret, 0);
//                 println!("Server: context switch returned");
//
//                 // Process any events from the context switch
//                 for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
//                     let switch_event_type = switch_event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
//                     let switch_worker_tid = switch_event >> UMCG_WORKER_ID_SHIFT;
//                     println!("Server: after switch got event type {} from worker {}",
//                              switch_event_type, switch_worker_tid);
//                 }
//             }
//         }
//     }
//
//     worker_handle.join().unwrap();
//     0
// }