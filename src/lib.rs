/*
This is our primary file where we're trying to get UMCG working with
a thread pool, and multiple servers with their own workers.
 */

#![allow(warnings)]
use crossbeam_queue::ArrayQueue;
use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
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

fn debug_worker_id(worker_id: u64) -> String {
    let server_id = (worker_id >> 32) as u32;
    let tid = (worker_id & 0xFFFFFFFF) as u32;
    format!("server:{} tid:{} (raw:{})", server_id, tid, worker_id)
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
    pending: ArrayQueue<TaskEntry>,
    preempted: ArrayQueue<TaskEntry>,
    in_progress: HashMap<Uuid, TaskEntry>,
    mutex: Mutex<()>,
}

impl TaskQueue {
    fn new() -> Self {
        info!("Creating new TaskQueue");
        Self {
            pending: ArrayQueue::new(1024),
            preempted: ArrayQueue::new(1024),
            in_progress: HashMap::new(),
            mutex: Mutex::new(()),
        }
    }

    fn enqueue(&mut self, task: TaskEntry) {
        info!("TaskQueue: Enqueueing task {}", task.id);
        match &task.state {
            TaskState::Pending(_) => {
                // No more blocking on mutex!
                if let Err(_) = self.pending.push(task) {
                    error!("Queue full - task dropped!");
                }
            },
            TaskState::Running { preempted: true, .. } => {
                if let Err(_) = self.preempted.push(task) {
                    error!("Preempted queue full!");
                }
            },
            TaskState::Running { .. } => {
                self.in_progress.insert(task.id, task);
            }
            TaskState::Completed => {
                info!("Attempting to enqueue completed task {}", task.id);
            }
        }
        debug!("TaskQueue stats - Pending: {}, Preempted: {}, In Progress: {}",
               self.pending.len(), self.preempted.len(), self.in_progress.len());
    }

    fn get_next_task(&mut self) -> Option<TaskEntry> {
        debug!("TaskQueue: Attempting to get next task");
        let task = self.preempted.pop()
            .or_else(|| self.pending.pop());

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

            // register workers with just the one server
            // let worker_id = (((self.server_id as u64) << 32) | (self.tid as u64)) << UMCG_WORKER_ID_SHIFT;
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

            // debug!("Worker {}: Entering task processing loop", self.id);
            // while let Ok(task) = self.task_rx.recv() {
            //     debug!("!!!!!!!!!! WORKER {}: Received task from channel !!!!!!!!!!!", self.id);
            //     match task {
            //         Task::Function(task) => {
            //             info!("!!!!!!!!!! WORKER {}: Starting task execution !!!!!!!!!!!", self.id);
            //             task(&self.handle);
            //             info!("!!!!!!!!!! WORKER {}: COMPLETED task execution !!!!!!!!!!!", self.id);
            //
            //             debug!("!!!!!!!!!! WORKER {} [{}]: Signaling ready for more work !!!!!!!!!!!",
            //             self.id, self.tid);
            //             let wait_result = sys_umcg_ctl(
            //                 self.server_id as u64,
            //                 UmcgCmd::Wait,
            //                 0,
            //                 0,
            //                 None,
            //                 0
            //             );
            //             debug!("!!!!!!!!!! WORKER {} [{}]: Wait syscall returned {} !!!!!!!!!!!",
            //             self.id, self.tid, wait_result);
            //             assert_eq!(wait_result, 0, "Worker {} UMCG wait failed", self.id);
            //         }
            //         Task::Shutdown => {
            //             info!("Worker {}: Shutting down", self.id);
            //             break;
            //         }
            //     }
            // }
            debug!("Worker {}: Entering task processing loop", self.id);
            loop {
                match self.task_rx.try_recv() {
                    Ok(Task::Function(task)) => {
                        info!("!!!!!!!!!! WORKER {}: Starting task execution !!!!!!!!!!!", self.id);
                        task(&self.handle);
                        info!("!!!!!!!!!! WORKER {}: COMPLETED task execution !!!!!!!!!!!", self.id);

                        debug!("!!!!!!!!!! WORKER {} [{}]: Signaling ready for more work !!!!!!!!!!!",
                self.id, self.tid);

                        // After completing a task, wait for more work
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
                    Ok(Task::Shutdown) => {
                        info!("Worker {}: Shutting down", self.id);
                        break;
                    }
                    Err(TryRecvError::Empty) => {
                        // No tasks available, go into wait state
                        let wait_result = sys_umcg_ctl(
                            self.server_id as u64,
                            UmcgCmd::Wait,
                            0,
                            0,
                            None,
                            0
                        );
                        debug!("!!!!!!!!!! WORKER {} [{}]: Wait returned {} (no tasks) !!!!!!!!!!!",
                self.id, self.tid, wait_result);
                        assert_eq!(wait_result, 0, "Worker {} UMCG wait failed", self.id);
                    }
                    Err(TryRecvError::Disconnected) => {
                        info!("Worker {}: Channel disconnected, shutting down", self.id);
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

        for i in 0..self.worker_pool.total_workers {

            let worker_id = (self.id * self.worker_pool.total_workers) + i;
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
                                    debug!("!!!!!!!!!! WAKE: Found pending task {} for worker {} !!!!!!!!!!",next_task.id, worker_tid);

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

    fn start(self, ready_counter: Arc<AtomicUsize>) -> JoinHandle<()> {
        thread::spawn(move || {
            info!("Starting server {} initialization", self.id);

            // Signal this server is ready before running
            ready_counter.fetch_add(1, Ordering::SeqCst);
            info!("Server {} ready for tasks", self.id);

            // Run the existing server loop
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
        let server_ready = Arc::new(AtomicUsize::new(0));
        let total_servers = servers.len();

        // Start each server with the shared ready counter
        let mut handles = Vec::new();
        for server in servers.iter() {
            handles.push(server.clone().start(server_ready.clone()));
        }

        // Wait for all servers to be ready
        while server_ready.load(Ordering::SeqCst) < total_servers {
            thread::sleep(Duration::from_millis(10));
        }

        info!("All {} servers started and ready", total_servers);

        // Store handles if needed
        drop(handles);
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

CLAUDE READ THIS - IT'S VERY IMPORTANT
So our code for the run_dynamic_task_demo works sometimes, but then fails other times. There's either a deadlock
or something happening but I"m unable to trace it. From 3 different runs we can see it succeeds once,
get's to 7/12 (which means a child task was entered to the queue but not run) and then it stops,
then another run where none of the tasks succeed.


➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[542.559] Creating new Executor
[542.575] Creating Server 0 on CPU 0
[542.585] Creating Server 0
[542.588] Creating new TaskQueue
[542.595] Creating WorkerPool with capacity for 3 workers
[542.600] Starting executor...
[542.600] Executor: Starting servers
[542.628] Starting server 0 initialization
[542.630] Server 0 ready for tasks
[542.645] Successfully set CPU affinity to 0
[542.648] Server 0: Starting up
[542.650] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[542.653] All 1 servers started and ready
[542.656] UMCG syscall result: 0
[542.657] Server 0: UMCG registration complete
[542.658] Server 0: Initializing workers
[542.660] Server 0: Initializing worker 0
[542.663] Starting WorkerThread 0 for server 0
[542.674] Successfully set CPU affinity to 0
[542.676] Worker 0: Initialized with tid 4
[542.679] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[542.680] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[542.684] WorkerThread 0 started with tid 4 for server 0
[542.685] Creating new Worker 0 for server 0
[542.686] WorkerPool: Adding worker 0 with tid 4
[542.689] WorkerPool: Updating worker 0 status from Initializing to Registering
[542.690] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[542.691] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[542.693] UMCG syscall result: 0
[542.694] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[542.694] Worker 0: Registering worker event 130
[542.695] WorkerPool: Updating worker 0 status from Registering to Waiting
[542.696] Server 0: Worker 0 initialized successfully
[542.697] Server 0: Initializing worker 1
[542.699] Starting WorkerThread 1 for server 0
[542.708] Successfully set CPU affinity to 0
[542.708] Worker 1: Initialized with tid 5
[542.715] Submitting initial tasks...
[542.717] !!!! Submitting initial task 0 !!!!
[542.716] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[542.718] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[542.718] Executor: Submitting new task
[542.718] WorkerThread 1 started with tid 5 for server 0
[542.725] Creating new Worker 1 for server 0
[542.726] WorkerPool: Adding worker 1 with tid 5
[542.728] WorkerPool: Updating worker 1 status from Initializing to Registering
[542.729] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[542.729] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[542.731] UMCG syscall result: 0
[542.731] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[542.732] Worker 0: Registering worker event 162
[542.732] WorkerPool: Updating worker 1 status from Registering to Waiting
[542.732] Server 0: Worker 1 initialized successfully
[542.733] Server 0: Initializing worker 2
[542.735] Starting WorkerThread 2 for server 0
[542.738] Registered task ae3a41b1-d467-4569-93ff-b5dc816e1092, total tasks: 1
[542.741] Successfully set CPU affinity to 0
[542.741] Worker 2: Initialized with tid 6
[542.745] Executor: Adding task ae3a41b1-d467-4569-93ff-b5dc816e1092 to server 0 (has 2 workers)
[542.747] Server 0: Adding new task
[542.752] Creating new TaskEntry with ID: a1e9e021-4442-44e5-b2e9-11e4d0f4e68a
[542.752] WorkerThread 2 started with tid 6 for server 0
[542.759] TaskQueue: Enqueueing task a1e9e021-4442-44e5-b2e9-11e4d0f4e68a
[542.762] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[542.763] Server 0: Task queued
[542.764] Task ae3a41b1-d467-4569-93ff-b5dc816e1092 assigned to server 0
[542.765] !!!! Submitting initial task 1 !!!!
[542.765] Executor: Submitting new task
[542.766] Registered task 9082ab66-9e09-4825-b7c2-f6e68821edc5, total tasks: 2
[542.767] Executor: Adding task 9082ab66-9e09-4825-b7c2-f6e68821edc5 to server 0 (has 2 workers)
[542.772] Server 0: Adding new task
[542.773] Creating new TaskEntry with ID: a87dd849-8c8a-4c4f-a232-cf8112d9659c
[542.775] TaskQueue: Enqueueing task a87dd849-8c8a-4c4f-a232-cf8112d9659c
[542.775] Creating new Worker 2 for server 0
[542.776] WorkerPool: Adding worker 2 with tid 6
[542.776] WorkerPool: Updating worker 2 status from Initializing to Registering
[542.776] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[542.778] Server 0: Task queued
[542.777] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[542.760] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[542.778] Task 9082ab66-9e09-4825-b7c2-f6e68821edc5 assigned to server 0
[542.779] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[542.781] !!!! Submitting initial task 2 !!!!
[542.782] Executor: Submitting new task
[542.783] Registered task e3f49de4-2f56-46ef-8765-914bd259c2e0, total tasks: 3
[542.784] Executor: Adding task e3f49de4-2f56-46ef-8765-914bd259c2e0 to server 0 (has 3 workers)
[542.779] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[542.789] UMCG syscall result: 0
[542.789] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[542.790] Worker 0: Registering worker event 194
[542.790] WorkerPool: Updating worker 2 status from Registering to Waiting
[542.791] Server 0: Worker 2 initialized successfully
[542.784] Server 0: Adding new task
[542.791] Server 0: All workers initialized
[542.791] Creating new TaskEntry with ID: 77568469-a436-4e41-bd1d-d4f4364bfe3f
[542.793] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[542.793] TaskQueue: Enqueueing task 77568469-a436-4e41-bd1d-d4f4364bfe3f
[542.794] Server 0: Attempting to schedule tasks...
[542.794] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[542.794] Server 0: Task queued
[542.794] Task e3f49de4-2f56-46ef-8765-914bd259c2e0 assigned to server 0
[542.794] Server 0: Looking for waiting workers among 3 workers
[542.795] !!!! Submitting initial task 3 !!!!
[542.795] Executor: Submitting new task
[542.796] Server 0: Found waiting worker 0 with status Waiting
[542.797] Registered task 19d734ef-8ee7-46a2-9644-49aea6805906, total tasks: 4
[542.797] Executor: Adding task 19d734ef-8ee7-46a2-9644-49aea6805906 to server 0 (has 3 workers)
[542.798] Server 0: Adding new task
[542.797] TaskQueue: Attempting to get next task
[542.798] Creating new TaskEntry with ID: dfe72135-2137-4176-91b5-879b0bf1628a
[542.800] TaskQueue: Retrieved task a1e9e021-4442-44e5-b2e9-11e4d0f4e68a
[542.800] Server 0: Assigning task to worker 0
[542.800] WorkerPool: Updating worker 0 status from Waiting to Running
[542.801] TaskQueue: Marking task a1e9e021-4442-44e5-b2e9-11e4d0f4e68a as in progress with worker 4
[542.802] TaskQueue: Enqueueing task dfe72135-2137-4176-91b5-879b0bf1628a
[542.802] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[542.802] Server 0: Task queued
[542.802] Worker 0: Starting task assignment
[542.803] Task 19d734ef-8ee7-46a2-9644-49aea6805906 assigned to server 0
[542.803] !!!! Submitting initial task 4 !!!!
[542.803] Executor: Submitting new task
[542.803] Registered task 111f6d37-3fd6-4c24-9e65-ad2ee5d7cba9, total tasks: 5
[542.804] Executor: Adding task 111f6d37-3fd6-4c24-9e65-ad2ee5d7cba9 to server 0 (has 3 workers)
[542.804] Worker 0: Task sent successfully
[542.804] Server 0: Adding new task
[542.805] Creating new TaskEntry with ID: 04000185-bf7b-4a08-a37f-81eef786bd51
[542.805] Server 0: Context switching to worker 4
[542.806] TaskQueue: Enqueueing task 04000185-bf7b-4a08-a37f-81eef786bd51
[542.806] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[542.806] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[542.807] Server 0: Task queued
[542.807] Task 111f6d37-3fd6-4c24-9e65-ad2ee5d7cba9 assigned to server 0
[542.808] !!!! Submitting initial task 5 !!!!
[542.808] Executor: Submitting new task
[542.809] Registered task 2edc0f98-cdde-440f-92e2-a0ce191378f4, total tasks: 6
[542.809] Executor: Adding task 2edc0f98-cdde-440f-92e2-a0ce191378f4 to server 0 (has 3 workers)
[542.809] Server 0: Adding new task
[542.809] Creating new TaskEntry with ID: dae1b394-dae8-468f-8336-9a8311af4b50
[542.810] TaskQueue: Enqueueing task dae1b394-dae8-468f-8336-9a8311af4b50
[542.810] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[542.810] Server 0: Task queued
[542.811] Task 2edc0f98-cdde-440f-92e2-a0ce191378f4 assigned to server 0
[542.811] All tasks submitted, waiting for completion...
[542.811] UMCG syscall result: 0
[542.813] Worker 0: UMCG registration complete with server 0
[542.814] Worker 0: Entering task processing loop
[542.814] Progress: 0/6 tasks completed
[542.815] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[542.816] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[542.816] !!!! Initial task 0: STARTING task !!!!
[542.816] !!!! Initial task 0: ABOUT TO SLEEP !!!!
[542.818] UMCG syscall result: 0
[542.819] Server 0: Processing event 1 for worker 4
[542.819] Server 0: Worker 4 blocked
[542.821] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(a1e9e021-4442-44e5-b2e9-11e4d0f4e68a))
[542.822] Server 0: Successfully switched to worker 4
[542.826] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[542.826] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[542.925] Progress: 0/6 tasks completed
[543.048] Progress: 0/6 tasks completed
[543.156] Progress: 0/6 tasks completed
[543.275] Progress: 0/6 tasks completed
[543.379] Progress: 0/6 tasks completed
[543.488] Progress: 0/6 tasks completed
[543.594] Progress: 0/6 tasks completed
[543.704] Progress: 0/6 tasks completed
[543.812] Progress: 0/6 tasks completed
en1: assigned FE80::84CE:5FF:FE4A:B67B
[544.834] UMCG syscall result: 0
[544.835] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[544.837] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[544.838] Server 0: Processing event 2 for worker 4
[544.839] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[544.840] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[544.842] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[544.842] Server 0: Worker 4 unblocking
[544.843] WorkerPool: Updating worker 0 status from Blocked to Running
[544.843] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[544.844] Server 0: Context switching to worker 4
[544.844] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[544.846] !!!! Initial task 0: WOKE UP FROM SLEEP !!!!
[544.847] !!!! Initial task 0: PREPARING to spawn child task !!!!
[544.847] TaskHandle: Submitting new task
[544.848] Executor: Submitting new task
[544.848] Registered task 3d4533ab-fbd5-4107-a47d-bcdd1b77c667, total tasks: 7
[544.849] Executor: Adding task 3d4533ab-fbd5-4107-a47d-bcdd1b77c667 to server 0 (has 3 workers)
[544.850] Server 0: Adding new task
[544.850] Creating new TaskEntry with ID: c0f29838-5315-4ab0-9426-f17c52bc8b17
[544.850] TaskQueue: Enqueueing task c0f29838-5315-4ab0-9426-f17c52bc8b17
[544.851] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[544.851] Server 0: Task queued
[544.852] Task 3d4533ab-fbd5-4107-a47d-bcdd1b77c667 assigned to server 0
[544.854] UMCG syscall result: 0
[544.854] Server 0: Processing event 1 for worker 4
[544.854] Server 0: Worker 4 blocked
[544.856] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(a1e9e021-4442-44e5-b2e9-11e4d0f4e68a))
[544.857] Server 0: Attempting to schedule tasks...
[544.858] Server 0: Looking for waiting workers among 3 workers
[544.858] Server 0: Found waiting worker 1 with status Waiting
[544.859] TaskQueue: Attempting to get next task
[544.859] TaskQueue: Retrieved task a87dd849-8c8a-4c4f-a232-cf8112d9659c
[544.860] Server 0: Assigning task to worker 1
[544.860] WorkerPool: Updating worker 1 status from Waiting to Running
[544.861] TaskQueue: Marking task a87dd849-8c8a-4c4f-a232-cf8112d9659c as in progress with worker 5
[544.861] Worker 1: Starting task assignment
[544.862] Worker 1: Task sent successfully
[544.863] Server 0: Context switching to worker 5
[544.863] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[544.865] UMCG syscall result: 0
[544.865] Worker 1: UMCG registration complete with server 0
[544.865] Worker 1: Entering task processing loop
[544.867] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[544.872] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[544.873] !!!! Initial task 1: STARTING task !!!!
[544.873] !!!! Initial task 1: ABOUT TO SLEEP !!!!
[544.874] UMCG syscall result: 0
[544.874] Server 0: Processing event 1 for worker 5
[544.874] Server 0: Worker 5 blocked
[544.875] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(a87dd849-8c8a-4c4f-a232-cf8112d9659c))
[544.876] Server 0: Successfully switched to worker 5
[544.877] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[544.879] Server 0: Attempting to schedule tasks...
[544.879] Server 0: Looking for waiting workers among 3 workers
[544.879] Server 0: Found waiting worker 2 with status Waiting
[544.880] TaskQueue: Attempting to get next task
[544.880] TaskQueue: Retrieved task 77568469-a436-4e41-bd1d-d4f4364bfe3f
[544.880] Server 0: Assigning task to worker 2
[544.880] WorkerPool: Updating worker 2 status from Waiting to Running
[544.881] TaskQueue: Marking task 77568469-a436-4e41-bd1d-d4f4364bfe3f as in progress with worker 6
[544.881] Worker 2: Starting task assignment
[544.883] Worker 2: Task sent successfully
[544.883] Server 0: Context switching to worker 6
[544.883] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[544.884] UMCG syscall result: 0
[544.884] Worker 2: UMCG registration complete with server 0
[544.884] Worker 2: Entering task processing loop
[544.885] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[544.885] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[544.885] !!!! Initial task 2: STARTING task !!!!
[544.885] !!!! Initial task 2: ABOUT TO SLEEP !!!!
[544.886] UMCG syscall result: 0
[544.886] Server 0: Processing event 1 for worker 6
[544.887] Server 0: Worker 6 blocked
[544.887] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(77568469-a436-4e41-bd1d-d4f4364bfe3f))
[544.888] Server 0: Successfully switched to worker 6
[544.888] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[544.889] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[544.889] UMCG syscall result: 0
[544.889] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[544.890] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[544.890] Server 0: Processing event 2 for worker 4
[544.890] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[544.891] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[544.891] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[544.892] Server 0: Worker 4 unblocking
[544.892] WorkerPool: Updating worker 0 status from Blocked to Running
[544.892] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[544.892] Server 0: Context switching to worker 4
[544.893] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[544.893] TaskHandle: Task submitted successfully
[544.894] !!!! Initial task 0: COMPLETED !!!!
[544.895] Completed task ae3a41b1-d467-4569-93ff-b5dc816e1092, total completed: 1/7
[544.895] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[544.896] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[544.896] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[544.897] UMCG syscall result: 0
[544.897] Server 0: Processing event 3 for worker 4
[544.897] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[544.898] Server 0: Got explicit WAIT from worker 4
[544.898] Server 0: Worker 4 current status: Running
[544.899] WorkerPool: Updating worker 0 status from Running to Waiting
[544.899] Server 0: Attempting to schedule tasks...
[544.899] Server 0: Looking for waiting workers among 3 workers
[544.899] Server 0: Found waiting worker 0 with status Waiting
[544.899] TaskQueue: Attempting to get next task
[544.900] TaskQueue: Retrieved task dfe72135-2137-4176-91b5-879b0bf1628a
[544.900] Server 0: Assigning task to worker 0
[544.900] WorkerPool: Updating worker 0 status from Waiting to Running
[544.900] TaskQueue: Marking task dfe72135-2137-4176-91b5-879b0bf1628a as in progress with worker 4
[544.901] Worker 0: Starting task assignment
[544.901] Worker 0: Task sent successfully
[544.901] Server 0: Context switching to worker 4
[544.901] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[544.902] UMCG syscall result: 0
[544.902] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[544.903] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[544.903] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[544.903] !!!! Initial task 3: STARTING task !!!!
[544.904] !!!! Initial task 3: ABOUT TO SLEEP !!!!
[544.904] UMCG syscall result: 0
[544.904] Server 0: Processing event 1 for worker 4
[544.904] Server 0: Worker 4 blocked
[544.905] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(dfe72135-2137-4176-91b5-879b0bf1628a))
[544.906] Server 0: Successfully switched to worker 4
[544.906] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[544.906] Server 0: Attempting to schedule tasks...
[544.907] Server 0: Looking for waiting workers among 3 workers
[544.907] Server 0: No waiting workers found
[544.908] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[544.908] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.882] UMCG syscall result: 0
[546.883] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[546.885] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[546.885] Server 0: Processing event 2 for worker 5
[546.886] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.887] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[546.888] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.889] Server 0: Worker 5 unblocking
[546.889] WorkerPool: Updating worker 1 status from Blocked to Running
[546.889] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[546.891] Server 0: Context switching to worker 5
[546.891] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[546.892] !!!! Initial task 1: WOKE UP FROM SLEEP !!!!
[546.893] !!!! Initial task 1: PREPARING to spawn child task !!!!
[546.893] TaskHandle: Submitting new task
[546.894] Executor: Submitting new task
[546.895] Registered task 50ba484a-d3cf-4d4f-8ffc-8e9c7a896b54, total tasks: 8
[546.896] Executor: Adding task 50ba484a-d3cf-4d4f-8ffc-8e9c7a896b54 to server 0 (has 3 workers)
[546.897] Server 0: Adding new task
[546.897] Creating new TaskEntry with ID: 7a2808df-58d1-4f35-b8a0-9c31a3e2af62
[546.897] TaskQueue: Enqueueing task 7a2808df-58d1-4f35-b8a0-9c31a3e2af62
[546.898] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[546.899] Server 0: Task queued
[546.899] Task 50ba484a-d3cf-4d4f-8ffc-8e9c7a896b54 assigned to server 0
[546.900] UMCG syscall result: 0
[546.900] Server 0: Processing event 1 for worker 5
[546.901] Server 0: Worker 5 blocked
[546.901] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(a87dd849-8c8a-4c4f-a232-cf8112d9659c))
[546.901] Server 0: Attempting to schedule tasks...
[546.902] Server 0: Looking for waiting workers among 3 workers
[546.903] Server 0: No waiting workers found
[546.903] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[546.904] Server 0: Attempting to schedule tasks...
[546.904] Server 0: Looking for waiting workers among 3 workers
[546.906] Server 0: No waiting workers found
[546.906] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[546.907] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.907] UMCG syscall result: 0
[546.907] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[546.908] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[546.908] Server 0: Processing event 2 for worker 6
[546.909] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.909] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[546.910] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.910] Server 0: Worker 6 unblocking
[546.911] WorkerPool: Updating worker 2 status from Blocked to Running
[546.912] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[546.912] Server 0: Context switching to worker 6
[546.913] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[546.913] !!!! Initial task 2: WOKE UP FROM SLEEP !!!!
[546.913] !!!! Initial task 2: PREPARING to spawn child task !!!!
[546.914] TaskHandle: Submitting new task
[546.914] Executor: Submitting new task
[546.914] Registered task d4f97c1f-c6e0-43e6-947b-0c9d724ab4e5, total tasks: 9
[546.915] Executor: Adding task d4f97c1f-c6e0-43e6-947b-0c9d724ab4e5 to server 0 (has 3 workers)
[546.915] Server 0: Adding new task
[546.916] Creating new TaskEntry with ID: 5cd4f314-5942-4ece-9950-f7ff64ed1ba9
[546.916] TaskQueue: Enqueueing task 5cd4f314-5942-4ece-9950-f7ff64ed1ba9
[546.916] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[546.917] Server 0: Task queued
[546.917] Task d4f97c1f-c6e0-43e6-947b-0c9d724ab4e5 assigned to server 0
[546.918] UMCG syscall result: 0
[546.918] Server 0: Processing event 1 for worker 6
[546.918] Server 0: Worker 6 blocked
[546.919] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(77568469-a436-4e41-bd1d-d4f4364bfe3f))
[546.919] Server 0: Attempting to schedule tasks...
[546.919] Server 0: Looking for waiting workers among 3 workers
[546.920] Server 0: No waiting workers found
[546.920] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[546.921] Server 0: Processing event 2 for worker 4
[546.921] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.921] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[546.922] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.923] Server 0: Worker 4 unblocking
[546.923] WorkerPool: Updating worker 0 status from Blocked to Running
[546.924] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[546.924] Server 0: Context switching to worker 4
[546.925] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[546.925] !!!! Initial task 3: WOKE UP FROM SLEEP !!!!
[546.926] !!!! Initial task 3: PREPARING to spawn child task !!!!
[546.926] TaskHandle: Submitting new task
[546.926] Executor: Submitting new task
[546.928] Registered task b8ca2eec-52ba-49f1-a03b-4db6f11aff18, total tasks: 10
[546.928] Executor: Adding task b8ca2eec-52ba-49f1-a03b-4db6f11aff18 to server 0 (has 3 workers)
[546.932] Server 0: Adding new task
[546.932] Creating new TaskEntry with ID: 3443d41a-fc0c-483f-afdc-f3982e7a9399
[546.932] TaskQueue: Enqueueing task 3443d41a-fc0c-483f-afdc-f3982e7a9399
[546.933] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[546.933] Server 0: Task queued
[546.933] Task b8ca2eec-52ba-49f1-a03b-4db6f11aff18 assigned to server 0
[546.934] UMCG syscall result: 0
[546.934] Server 0: Processing event 1 for worker 4
[546.934] Server 0: Worker 4 blocked
[546.935] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(dfe72135-2137-4176-91b5-879b0bf1628a))
[546.935] Server 0: Attempting to schedule tasks...
[546.935] Server 0: Looking for waiting workers among 3 workers
[546.936] Server 0: No waiting workers found
[546.936] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[546.936] Server 0: Attempting to schedule tasks...
[546.936] Server 0: Looking for waiting workers among 3 workers
[546.936] Server 0: No waiting workers found
[546.937] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[546.937] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.937] UMCG syscall result: 0
[546.938] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[546.938] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[546.938] Server 0: Processing event 2 for worker 5
[546.938] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.939] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[546.939] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.939] Server 0: Worker 5 unblocking
[546.940] WorkerPool: Updating worker 1 status from Blocked to Running
[546.941] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[546.941] Server 0: Context switching to worker 5
[546.941] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[546.942] TaskHandle: Task submitted successfully
[546.942] !!!! Initial task 1: COMPLETED !!!!
[546.943] Completed task 9082ab66-9e09-4825-b7c2-f6e68821edc5, total completed: 2/10
[546.943] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[546.943] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[546.944] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.944] UMCG syscall result: 0
[546.945] Server 0: Processing event 3 for worker 5
[546.945] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[546.945] Server 0: Got explicit WAIT from worker 5
[546.945] Server 0: Worker 5 current status: Running
[546.945] WorkerPool: Updating worker 1 status from Running to Waiting
[546.946] Server 0: Attempting to schedule tasks...
[546.946] Server 0: Looking for waiting workers among 3 workers
[546.946] Server 0: Found waiting worker 1 with status Waiting
[546.947] TaskQueue: Attempting to get next task
[546.947] TaskQueue: Retrieved task 04000185-bf7b-4a08-a37f-81eef786bd51
[546.947] Server 0: Assigning task to worker 1
[546.948] WorkerPool: Updating worker 1 status from Waiting to Running
[546.948] TaskQueue: Marking task 04000185-bf7b-4a08-a37f-81eef786bd51 as in progress with worker 5
[546.948] Worker 1: Starting task assignment
[546.948] Worker 1: Task sent successfully
[546.949] Server 0: Context switching to worker 5
[546.949] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[546.949] UMCG syscall result: 0
[546.949] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[546.950] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[546.950] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[546.950] !!!! Initial task 4: STARTING task !!!!
[546.950] !!!! Initial task 4: ABOUT TO SLEEP !!!!
[546.950] UMCG syscall result: 0
[546.951] Server 0: Processing event 1 for worker 5
[546.951] Server 0: Worker 5 blocked
[546.951] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(04000185-bf7b-4a08-a37f-81eef786bd51))
[546.952] Server 0: Successfully switched to worker 5
[546.952] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[546.952] Server 0: Processing event 2 for worker 6
[546.952] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.953] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[546.953] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.953] Server 0: Worker 6 unblocking
[546.954] WorkerPool: Updating worker 2 status from Blocked to Running
[546.954] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[546.954] Server 0: Context switching to worker 6
[546.954] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[546.955] TaskHandle: Task submitted successfully
[546.955] !!!! Initial task 2: COMPLETED !!!!
[546.955] Completed task e3f49de4-2f56-46ef-8765-914bd259c2e0, total completed: 3/10
[546.956] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[546.956] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[546.956] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.956] UMCG syscall result: 0
[546.956] Server 0: Processing event 3 for worker 6
[546.957] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[546.957] Server 0: Got explicit WAIT from worker 6
[546.958] Server 0: Worker 6 current status: Running
[546.958] WorkerPool: Updating worker 2 status from Running to Waiting
[546.959] Server 0: Attempting to schedule tasks...
[546.959] Server 0: Looking for waiting workers among 3 workers
[546.959] Server 0: Found waiting worker 2 with status Waiting
[546.960] TaskQueue: Attempting to get next task
[546.960] TaskQueue: Retrieved task dae1b394-dae8-468f-8336-9a8311af4b50
[546.961] Server 0: Assigning task to worker 2
[546.961] WorkerPool: Updating worker 2 status from Waiting to Running
[546.961] TaskQueue: Marking task dae1b394-dae8-468f-8336-9a8311af4b50 as in progress with worker 6
[546.961] Worker 2: Starting task assignment
[546.962] Worker 2: Task sent successfully
[546.962] Server 0: Context switching to worker 6
[546.962] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[546.962] UMCG syscall result: 0
[546.962] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[546.963] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[546.964] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[546.964] !!!! Initial task 5: STARTING task !!!!
[546.964] !!!! Initial task 5: ABOUT TO SLEEP !!!!
[546.964] UMCG syscall result: 0
[546.964] Server 0: Processing event 1 for worker 6
[546.965] Server 0: Worker 6 blocked
[546.965] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(dae1b394-dae8-468f-8336-9a8311af4b50))
[546.965] Server 0: Successfully switched to worker 6
[546.965] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[546.966] Server 0: Attempting to schedule tasks...
[546.966] Server 0: Looking for waiting workers among 3 workers
[546.966] Server 0: No waiting workers found
[546.967] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[546.967] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.967] UMCG syscall result: 0
[546.967] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[546.968] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[546.968] Server 0: Processing event 2 for worker 4
[546.968] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[546.969] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[546.969] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[546.969] Server 0: Worker 4 unblocking
[546.970] WorkerPool: Updating worker 0 status from Blocked to Running
[546.970] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[546.970] Server 0: Context switching to worker 4
[546.971] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[546.971] TaskHandle: Task submitted successfully
[546.972] !!!! Initial task 3: COMPLETED !!!!
[546.972] Completed task 19d734ef-8ee7-46a2-9644-49aea6805906, total completed: 4/10
[546.972] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[546.972] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[546.973] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[546.973] UMCG syscall result: 0
[546.973] Server 0: Processing event 3 for worker 4
[546.973] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[546.974] Server 0: Got explicit WAIT from worker 4
[546.974] Server 0: Worker 4 current status: Running
[546.975] WorkerPool: Updating worker 0 status from Running to Waiting
[546.975] Server 0: Attempting to schedule tasks...
[546.975] Server 0: Looking for waiting workers among 3 workers
[546.975] Server 0: Found waiting worker 0 with status Waiting
[546.976] TaskQueue: Attempting to get next task
[546.976] TaskQueue: Retrieved task c0f29838-5315-4ab0-9426-f17c52bc8b17
[546.976] Server 0: Assigning task to worker 0
[546.976] WorkerPool: Updating worker 0 status from Waiting to Running
[546.977] TaskQueue: Marking task c0f29838-5315-4ab0-9426-f17c52bc8b17 as in progress with worker 4
[546.977] Worker 0: Starting task assignment
[546.977] Worker 0: Task sent successfully
[546.978] Server 0: Context switching to worker 4
[546.978] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[546.979] UMCG syscall result: 0
[546.979] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[546.979] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[546.980] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[546.981] !!!! Child task of initial task 0: STARTING work !!!!
[546.981] UMCG syscall result: 0
[546.981] Server 0: Processing event 1 for worker 4
[546.981] Server 0: Worker 4 blocked
[546.982] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(c0f29838-5315-4ab0-9426-f17c52bc8b17))
[546.983] Server 0: Successfully switched to worker 4
[546.983] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[546.983] Server 0: Attempting to schedule tasks...
[546.984] Server 0: Looking for waiting workers among 3 workers
[546.984] Server 0: No waiting workers found
[546.984] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[546.985] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[547.832] Progress: 4/10 tasks completed
[547.935] Progress: 4/10 tasks completed
[547.988] UMCG syscall result: 0
[547.989] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[547.990] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[547.992] Server 0: Processing event 2 for worker 4
[547.993] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[547.993] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[547.993] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[547.994] Server 0: Worker 4 unblocking
[547.995] WorkerPool: Updating worker 0 status from Blocked to Running
[547.995] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[547.997] Server 0: Context switching to worker 4
[547.998] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[547.999] !!!! Child task of initial task 0: COMPLETED !!!!
[548.001] Completed task 3d4533ab-fbd5-4107-a47d-bcdd1b77c667, total completed: 5/10
[548.003] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[548.003] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[548.004] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[548.005] UMCG syscall result: 0
[548.006] Server 0: Processing event 3 for worker 4
[548.006] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[548.007] Server 0: Got explicit WAIT from worker 4
[548.007] Server 0: Worker 4 current status: Running
[548.008] WorkerPool: Updating worker 0 status from Running to Waiting
[548.009] Server 0: Attempting to schedule tasks...
[548.009] Server 0: Looking for waiting workers among 3 workers
[548.009] Server 0: Found waiting worker 0 with status Waiting
[548.010] TaskQueue: Attempting to get next task
[548.011] TaskQueue: Retrieved task 7a2808df-58d1-4f35-b8a0-9c31a3e2af62
[548.012] Server 0: Assigning task to worker 0
[548.012] WorkerPool: Updating worker 0 status from Waiting to Running
[548.012] TaskQueue: Marking task 7a2808df-58d1-4f35-b8a0-9c31a3e2af62 as in progress with worker 4
[548.013] Worker 0: Starting task assignment
[548.013] Worker 0: Task sent successfully
[548.014] Server 0: Context switching to worker 4
[548.014] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[548.015] UMCG syscall result: 0
[548.015] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[548.015] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[548.016] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[548.017] !!!! Child task of initial task 1: STARTING work !!!!
[548.019] UMCG syscall result: 0
[548.019] Server 0: Processing event 1 for worker 4
[548.019] Server 0: Worker 4 blocked
[548.022] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(7a2808df-58d1-4f35-b8a0-9c31a3e2af62))
[548.023] Server 0: Successfully switched to worker 4
[548.023] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[548.024] Server 0: Attempting to schedule tasks...
[548.024] Server 0: Looking for waiting workers among 3 workers
[548.024] Server 0: No waiting workers found
[548.024] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[548.025] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[548.039] Progress: 5/10 tasks completed
[548.143] Progress: 5/10 tasks completed
[548.247] Progress: 5/10 tasks completed
[548.353] Progress: 5/10 tasks completed
[548.460] Progress: 5/10 tasks completed
[548.564] Progress: 5/10 tasks completed
[548.672] Progress: 5/10 tasks completed
[548.776] Progress: 5/10 tasks completed
[548.957] UMCG syscall result: 0
[548.957] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[548.957] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[548.958] Server 0: Processing event 2 for worker 5
[548.958] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[548.959] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[548.960] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[548.960] Server 0: Worker 5 unblocking
[548.960] WorkerPool: Updating worker 1 status from Blocked to Running
[548.961] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[548.961] Server 0: Context switching to worker 5
[548.962] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[548.962] !!!! Initial task 4: WOKE UP FROM SLEEP !!!!
[548.962] !!!! Initial task 4: PREPARING to spawn child task !!!!
[548.963] TaskHandle: Submitting new task
[548.963] Executor: Submitting new task
[548.964] Registered task 5cb3c5ac-09c5-4870-bf26-86f3d85c338d, total tasks: 11
[548.964] Executor: Adding task 5cb3c5ac-09c5-4870-bf26-86f3d85c338d to server 0 (has 3 workers)
[548.966] Server 0: Adding new task
[548.966] Creating new TaskEntry with ID: 51338b8d-e191-42f0-9668-364863e6b4b4
[548.967] TaskQueue: Enqueueing task 51338b8d-e191-42f0-9668-364863e6b4b4
[548.967] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[548.967] Server 0: Task queued
[548.968] Task 5cb3c5ac-09c5-4870-bf26-86f3d85c338d assigned to server 0
[548.969] UMCG syscall result: 0
[548.969] Server 0: Processing event 1 for worker 5
[548.969] Server 0: Worker 5 blocked
[548.969] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(04000185-bf7b-4a08-a37f-81eef786bd51))
[548.970] Server 0: Attempting to schedule tasks...
[548.970] Server 0: Looking for waiting workers among 3 workers
[548.970] Server 0: No waiting workers found
[548.971] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[548.971] Server 0: Attempting to schedule tasks...
[548.971] Server 0: Looking for waiting workers among 3 workers
[548.972] Server 0: No waiting workers found
[548.972] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[548.972] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[548.973] UMCG syscall result: 0
[548.973] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[548.973] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[548.974] Server 0: Processing event 2 for worker 6
[548.974] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[548.974] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[548.975] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[548.976] Server 0: Worker 6 unblocking
[548.976] WorkerPool: Updating worker 2 status from Blocked to Running
[548.977] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[548.977] Server 0: Context switching to worker 6
[548.977] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[548.978] !!!! Initial task 5: WOKE UP FROM SLEEP !!!!
[548.978] !!!! Initial task 5: PREPARING to spawn child task !!!!
[548.979] TaskHandle: Submitting new task
[548.979] Executor: Submitting new task
[548.981] Registered task 92d09c7e-5210-4250-9d63-c218987aa6dd, total tasks: 12
[548.981] Executor: Adding task 92d09c7e-5210-4250-9d63-c218987aa6dd to server 0 (has 3 workers)
[548.984] Server 0: Adding new task
[548.984] Creating new TaskEntry with ID: 15d8d014-6da3-421e-8d83-adc1b7f5f371
[548.985] TaskQueue: Enqueueing task 15d8d014-6da3-421e-8d83-adc1b7f5f371
[548.985] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[548.985] Server 0: Task queued
[548.985] Task 92d09c7e-5210-4250-9d63-c218987aa6dd assigned to server 0
[548.986] UMCG syscall result: 0
[548.986] Server 0: Processing event 1 for worker 6
[548.987] Server 0: Worker 6 blocked
[548.987] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(dae1b394-dae8-468f-8336-9a8311af4b50))
[548.988] Server 0: Attempting to schedule tasks...
[548.988] Server 0: Looking for waiting workers among 3 workers
[548.989] Server 0: No waiting workers found
[548.989] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[548.990] Server 0: Attempting to schedule tasks...
[548.990] Server 0: Looking for waiting workers among 3 workers
[548.990] Server 0: No waiting workers found
[548.991] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[548.991] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[548.992] UMCG syscall result: 0
[548.992] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[548.992] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[548.993] Server 0: Processing event 2 for worker 5
[548.993] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[548.994] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[548.995] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[548.995] Server 0: Worker 5 unblocking
[548.995] WorkerPool: Updating worker 1 status from Blocked to Running
[548.996] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[548.996] Server 0: Context switching to worker 5
[548.996] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[548.997] TaskHandle: Task submitted successfully
[548.997] !!!! Initial task 4: COMPLETED !!!!
[548.997] Completed task 111f6d37-3fd6-4c24-9e65-ad2ee5d7cba9, total completed: 6/12
[548.998] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[548.998] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[548.998] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[548.999] UMCG syscall result: 0
[548.999] Server 0: Processing event 3 for worker 5
[548.999] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[549.000] Server 0: Got explicit WAIT from worker 5
[549.000] Server 0: Worker 5 current status: Running
[549.000] WorkerPool: Updating worker 1 status from Running to Waiting
[549.000] Server 0: Attempting to schedule tasks...
[549.001] Server 0: Looking for waiting workers among 3 workers
[549.001] Server 0: Found waiting worker 1 with status Waiting
[549.001] TaskQueue: Attempting to get next task
[549.001] TaskQueue: Retrieved task 5cd4f314-5942-4ece-9950-f7ff64ed1ba9
[549.002] Server 0: Assigning task to worker 1
[549.002] WorkerPool: Updating worker 1 status from Waiting to Running
[549.002] TaskQueue: Marking task 5cd4f314-5942-4ece-9950-f7ff64ed1ba9 as in progress with worker 5
[549.002] Worker 1: Starting task assignment
[549.003] Worker 1: Task sent successfully
[549.003] Server 0: Context switching to worker 5
[549.003] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[549.003] UMCG syscall result: 0
[549.003] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[549.004] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[549.004] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[549.004] !!!! Child task of initial task 2: STARTING work !!!!
[549.005] UMCG syscall result: 0
[549.005] Server 0: Processing event 1 for worker 5
[549.005] Server 0: Worker 5 blocked
[549.006] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(5cd4f314-5942-4ece-9950-f7ff64ed1ba9))
[549.006] Server 0: Successfully switched to worker 5
[549.006] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[549.007] Server 0: Attempting to schedule tasks...
[549.007] Server 0: Looking for waiting workers among 3 workers
[549.008] Server 0: No waiting workers found
[549.008] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[549.008] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[549.009] UMCG syscall result: 0
[549.009] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[549.009] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[549.009] Server 0: Processing event 2 for worker 6
[549.009] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[549.010] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[549.010] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[549.011] Server 0: Worker 6 unblocking
[549.011] WorkerPool: Updating worker 2 status from Blocked to Running
[549.011] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[549.011] Server 0: Context switching to worker 6
[549.011] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[549.012] TaskHandle: Task submitted successfully
[549.012] !!!! Initial task 5: COMPLETED !!!!
[549.012] Completed task 2edc0f98-cdde-440f-92e2-a0ce191378f4, total completed: 7/12
[549.013] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[549.014] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[549.014] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[549.014] UMCG syscall result: 0
[549.014] Server 0: Processing event 3 for worker 6
[549.014] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[549.015] Server 0: Got explicit WAIT from worker 6
[549.015] Server 0: Worker 6 current status: Running
[549.015] WorkerPool: Updating worker 2 status from Running to Waiting
[549.015] Server 0: Attempting to schedule tasks...
[549.015] Server 0: Looking for waiting workers among 3 workers
[549.016] Server 0: Found waiting worker 2 with status Waiting
[549.016] TaskQueue: Attempting to get next task
[549.016] TaskQueue: Retrieved task 3443d41a-fc0c-483f-afdc-f3982e7a9399
[549.017] Server 0: Assigning task to worker 2
[549.017] WorkerPool: Updating worker 2 status from Waiting to Running
[549.017] TaskQueue: Marking task 3443d41a-fc0c-483f-afdc-f3982e7a9399 as in progress with worker 6
[549.017] Worker 2: Starting task assignment
[549.019] Worker 2: Task sent successfully
[549.019] Server 0: Context switching to worker 6
[549.019] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[549.019] UMCG syscall result: 0
[549.019] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[549.020] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[549.021] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[549.021] !!!! Child task of initial task 3: STARTING work !!!!
[549.024] UMCG syscall result: 0
[549.024] Server 0: Processing event 1 for worker 6
[549.024] Server 0: Worker 6 blocked
[549.024] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(3443d41a-fc0c-483f-afdc-f3982e7a9399))
[549.025] Server 0: Successfully switched to worker 6
[549.026] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[549.026] Server 0: Attempting to schedule tasks...
[549.026] Server 0: Looking for waiting workers among 3 workers
[549.027] Server 0: No waiting workers found
[549.027] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[549.028] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[549.028] UMCG syscall result: 0
[549.028] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[549.029] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[549.029] Server 0: Processing event 2 for worker 4
[549.030] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[549.030] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[549.030] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[549.030] Server 0: Worker 4 unblocking
[549.030] WorkerPool: Updating worker 0 status from Blocked to Running
[549.031] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[549.031] Server 0: Context switching to worker 4
[549.031] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[549.032] !!!! Child task of initial task 1: COMPLETED !!!!
[549.032] Completed task 50ba484a-d3cf-4d4f-8ffc-8e9c7a896b54, total completed: 8/12
[549.032] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[549.032] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[549.033] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[549.033] UMCG syscall result: 0
[549.034] Server 0: Processing event 3 for worker 4
[549.034] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[549.034] Server 0: Got explicit WAIT from worker 4
[549.035] Server 0: Worker 4 current status: Running
[549.035] WorkerPool: Updating worker 0 status from Running to Waiting
[549.035] Server 0: Attempting to schedule tasks...
[549.036] Server 0: Looking for waiting workers among 3 workers
[549.036] Server 0: Found waiting worker 0 with status Waiting
[549.036] TaskQueue: Attempting to get next task
[549.037] TaskQueue: Retrieved task 51338b8d-e191-42f0-9668-364863e6b4b4
[549.037] Server 0: Assigning task to worker 0
[549.037] WorkerPool: Updating worker 0 status from Waiting to Running
[549.037] TaskQueue: Marking task 51338b8d-e191-42f0-9668-364863e6b4b4 as in progress with worker 4
[549.037] Worker 0: Starting task assignment
[549.038] Worker 0: Task sent successfully
[549.038] Server 0: Context switching to worker 4
[549.038] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[549.039] UMCG syscall result: 0
[549.039] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[549.039] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[549.040] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[549.040] !!!! Child task of initial task 4: STARTING work !!!!
[549.040] UMCG syscall result: 0
[549.040] Server 0: Processing event 1 for worker 4
[549.041] Server 0: Worker 4 blocked
[549.041] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(51338b8d-e191-42f0-9668-364863e6b4b4))
[549.041] Server 0: Successfully switched to worker 4
[549.041] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[549.042] Server 0: Attempting to schedule tasks...
[549.042] Server 0: Looking for waiting workers among 3 workers
[549.042] Server 0: No waiting workers found
[549.043] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[549.043] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[550.014] UMCG syscall result: 0
[550.015] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[550.015] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[550.016] Server 0: Processing event 2 for worker 5
[550.017] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[550.018] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[550.019] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[550.019] Server 0: Worker 5 unblocking
[550.019] WorkerPool: Updating worker 1 status from Blocked to Running
[550.021] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[550.024] Server 0: Context switching to worker 5
[550.024] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[550.025] !!!! Child task of initial task 2: COMPLETED !!!!
[550.025] Completed task d4f97c1f-c6e0-43e6-947b-0c9d724ab4e5, total completed: 9/12
[550.025] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[550.026] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[550.027] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[550.027] UMCG syscall result: 0
[550.027] Server 0: Processing event 3 for worker 5
[550.027] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[550.028] Server 0: Got explicit WAIT from worker 5
[550.029] Server 0: Worker 5 current status: Running
[550.029] WorkerPool: Updating worker 1 status from Running to Waiting
[550.030] Server 0: Attempting to schedule tasks...
[550.030] Server 0: Looking for waiting workers among 3 workers
[550.031] Server 0: Found waiting worker 1 with status Waiting
[550.031] TaskQueue: Attempting to get next task
[550.032] TaskQueue: Retrieved task 15d8d014-6da3-421e-8d83-adc1b7f5f371
[550.033] Server 0: Assigning task to worker 1
[550.033] WorkerPool: Updating worker 1 status from Waiting to Running
[550.034] TaskQueue: Marking task 15d8d014-6da3-421e-8d83-adc1b7f5f371 as in progress with worker 5
[550.034] Worker 1: Starting task assignment
[550.035] Worker 1: Task sent successfully
[550.035] Server 0: Context switching to worker 5
[550.035] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[550.036] UMCG syscall result: 0
[550.036] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[550.036] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[550.037] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[550.038] !!!! Child task of initial task 5: STARTING work !!!!
[550.039] UMCG syscall result: 0
[550.039] Server 0: Processing event 1 for worker 5
[550.039] Server 0: Worker 5 blocked
[550.039] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(15d8d014-6da3-421e-8d83-adc1b7f5f371))
[550.039] Server 0: Successfully switched to worker 5
[550.040] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[550.040] Server 0: Attempting to schedule tasks...
[550.040] Server 0: Looking for waiting workers among 3 workers
[550.040] Server 0: No waiting workers found
[550.042] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[550.042] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[550.043] UMCG syscall result: 0
[550.043] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[550.044] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[550.044] Server 0: Processing event 2 for worker 6
[550.045] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[550.046] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[550.046] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[550.047] Server 0: Worker 6 unblocking
[550.047] WorkerPool: Updating worker 2 status from Blocked to Running
[550.048] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[550.048] Server 0: Context switching to worker 6
[550.049] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[550.049] !!!! Child task of initial task 3: COMPLETED !!!!
[550.050] Completed task b8ca2eec-52ba-49f1-a03b-4db6f11aff18, total completed: 10/12
[550.050] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[550.051] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[550.051] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[550.052] UMCG syscall result: 0
[550.053] Server 0: Processing event 3 for worker 6
[550.053] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[550.053] Server 0: Got explicit WAIT from worker 6
[550.053] Server 0: Worker 6 current status: Running
[550.054] WorkerPool: Updating worker 2 status from Running to Waiting
[550.055] Server 0: Attempting to schedule tasks...
[550.055] Server 0: Looking for waiting workers among 3 workers
[550.055] Server 0: Found waiting worker 2 with status Waiting
[550.056] TaskQueue: Attempting to get next task
[550.056] TaskQueue: No tasks available
[550.057] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[550.057] Server 0: Processing event 2 for worker 4
[550.058] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[550.058] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[550.058] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[550.059] Server 0: Worker 4 unblocking
[550.059] WorkerPool: Updating worker 0 status from Blocked to Running
[550.060] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[550.060] Server 0: Context switching to worker 4
[550.060] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[550.061] !!!! Child task of initial task 4: COMPLETED !!!!
[550.061] Completed task 5cb3c5ac-09c5-4870-bf26-86f3d85c338d, total completed: 11/12
[550.062] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[550.062] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[550.063] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[550.063] UMCG syscall result: 0
[550.063] Server 0: Processing event 3 for worker 4
[550.063] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[550.063] Server 0: Got explicit WAIT from worker 4
[550.064] Server 0: Worker 4 current status: Running
[550.064] WorkerPool: Updating worker 0 status from Running to Waiting
[550.065] Server 0: Attempting to schedule tasks...
[550.065] Server 0: Looking for waiting workers among 3 workers
[550.066] Server 0: Found waiting worker 0 with status Waiting
[550.066] TaskQueue: Attempting to get next task
[550.066] TaskQueue: No tasks available
[550.067] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[550.067] Server 0: Attempting to schedule tasks...
[550.068] Server 0: Looking for waiting workers among 3 workers
[550.068] Server 0: Found waiting worker 0 with status Waiting
[550.068] TaskQueue: Attempting to get next task
[550.068] TaskQueue: No tasks available
[550.069] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[550.069] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[551.045] UMCG syscall result: 0
[551.047] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[551.049] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[551.049] Server 0: Processing event 2 for worker 5
[551.050] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[551.051] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[551.052] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[551.052] Server 0: Worker 5 unblocking
[551.053] WorkerPool: Updating worker 1 status from Blocked to Running
[551.054] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[551.054] Server 0: Context switching to worker 5
[551.055] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[551.056] !!!! Child task of initial task 5: COMPLETED !!!!
[551.056] Completed task 92d09c7e-5210-4250-9d63-c218987aa6dd, total completed: 12/12
[551.057] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[551.058] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[551.059] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[551.059] UMCG syscall result: 0
[551.059] Server 0: Processing event 3 for worker 5
[551.059] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[551.060] Server 0: Got explicit WAIT from worker 5
[551.061] Server 0: Worker 5 current status: Running
[551.061] WorkerPool: Updating worker 1 status from Running to Waiting
[551.062] Server 0: Attempting to schedule tasks...
[551.062] Server 0: Looking for waiting workers among 3 workers
[551.063] Server 0: Found waiting worker 0 with status Waiting
[551.064] TaskQueue: Attempting to get next task
[551.064] TaskQueue: No tasks available
[551.064] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[551.065] Server 0: Attempting to schedule tasks...
[551.065] Server 0: Looking for waiting workers among 3 workers
[551.065] Server 0: Found waiting worker 0 with status Waiting
[551.066] TaskQueue: Attempting to get next task
[551.066] TaskQueue: No tasks available
[551.067] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[551.068] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[551.083] All tasks completed successfully (12/12)
[551.084] Initiating executor shutdown...
[551.084] Server 0: Adding new task
[551.085] Server 0: Received shutdown signal
Running run_multi_server_demo




/// blocked at 7/12
Running run_dynamic_task_demo
[828.561] Creating new Executor
[828.576] Creating Server 0 on CPU 0
[828.585] Creating Server 0
[828.589] Creating new TaskQueue
[828.596] Creating WorkerPool with capacity for 3 workers
[828.600] Starting executor...
[828.601] Executor: Starting servers
[828.626] Starting server 0 initialization
[828.630] Server 0 ready for tasks
[828.648] Successfully set CPU affinity to 0
[828.652] Server 0: Starting up
[828.655] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[828.659] All 1 servers started and ready
[828.663] UMCG syscall result: 0
[828.665] Server 0: UMCG registration complete
[828.665] Server 0: Initializing workers
[828.666] Server 0: Initializing worker 0
[828.671] Starting WorkerThread 0 for server 0
[828.682] Successfully set CPU affinity to 0
[828.684] Worker 0: Initialized with tid 4
[828.686] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[828.688] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[828.691] WorkerThread 0 started with tid 4 for server 0
[828.694] Creating new Worker 0 for server 0
[828.695] WorkerPool: Adding worker 0 with tid 4
[828.698] WorkerPool: Updating worker 0 status from Initializing to Registering
[828.699] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[828.699] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.701] UMCG syscall result: 0
[828.702] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[828.702] Worker 0: Registering worker event 130
[828.703] WorkerPool: Updating worker 0 status from Registering to Waiting
[828.704] Server 0: Worker 0 initialized successfully
[828.704] Server 0: Initializing worker 1
[828.706] Starting WorkerThread 1 for server 0
[828.715] Successfully set CPU affinity to 0
[828.715] Worker 1: Initialized with tid 5
[828.722] Submitting initial tasks...
[828.723] !!!! Submitting initial task 0 !!!!
[828.725] Executor: Submitting new task
[828.725] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[828.724] WorkerThread 1 started with tid 5 for server 0
[828.726] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[828.727] Creating new Worker 1 for server 0
[828.730] WorkerPool: Adding worker 1 with tid 5
[828.731] WorkerPool: Updating worker 1 status from Initializing to Registering
[828.732] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[828.733] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.734] UMCG syscall result: 0
[828.734] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[828.735] Worker 0: Registering worker event 162
[828.735] WorkerPool: Updating worker 1 status from Registering to Waiting
[828.735] Server 0: Worker 1 initialized successfully
[828.736] Server 0: Initializing worker 2
[828.738] Starting WorkerThread 2 for server 0
[828.743] Registered task ae82ede3-6607-4cc2-8dac-ec5acc05f1a3, total tasks: 1
[828.743] Successfully set CPU affinity to 0
[828.744] Worker 2: Initialized with tid 6
[828.749] Executor: Adding task ae82ede3-6607-4cc2-8dac-ec5acc05f1a3 to server 0 (has 2 workers)
[828.751] WorkerThread 2 started with tid 6 for server 0
[828.752] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[828.752] Server 0: Adding new task
[828.760] Creating new TaskEntry with ID: 6044cc5a-b819-491f-a4f6-008c27c159dc
[828.764] Creating new Worker 2 for server 0
[828.764] WorkerPool: Adding worker 2 with tid 6
[828.765] WorkerPool: Updating worker 2 status from Initializing to Registering
[828.766] TaskQueue: Enqueueing task 6044cc5a-b819-491f-a4f6-008c27c159dc
[828.766] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[828.767] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[828.767] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.768] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[828.768] UMCG syscall result: 0
[828.770] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[828.770] Server 0: Task queued
[828.770] Worker 0: Registering worker event 194
[828.770] WorkerPool: Updating worker 2 status from Registering to Waiting
[828.771] Server 0: Worker 2 initialized successfully
[828.772] Server 0: All workers initialized
[828.773] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[828.773] Server 0: Attempting to schedule tasks...
[828.774] Server 0: Looking for waiting workers among 3 workers
[828.772] Task ae82ede3-6607-4cc2-8dac-ec5acc05f1a3 assigned to server 0
[828.775] Server 0: Found waiting worker 2 with status Waiting
[828.776] !!!! Submitting initial task 1 !!!!
[828.776] Executor: Submitting new task
[828.777] TaskQueue: Attempting to get next task
[828.778] Registered task 9d99075f-5df6-48f2-8b45-856842eb009f, total tasks: 2
[828.779] Executor: Adding task 9d99075f-5df6-48f2-8b45-856842eb009f to server 0 (has 3 workers)
[828.780] TaskQueue: Retrieved task 6044cc5a-b819-491f-a4f6-008c27c159dc
[828.780] Server 0: Adding new task
[828.782] Creating new TaskEntry with ID: f2042d95-6856-4ced-b37a-12956a90cb9a
[828.784] Server 0: Assigning task to worker 2
[828.785] WorkerPool: Updating worker 2 status from Waiting to Running
[828.785] TaskQueue: Marking task 6044cc5a-b819-491f-a4f6-008c27c159dc as in progress with worker 6
[828.786] Worker 2: Starting task assignment
[828.787] TaskQueue: Enqueueing task f2042d95-6856-4ced-b37a-12956a90cb9a
[828.788] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[828.789] Worker 2: Task sent successfully
[828.789] Server 0: Task queued
[828.789] Server 0: Context switching to worker 6
[828.790] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[828.790] Task 9d99075f-5df6-48f2-8b45-856842eb009f assigned to server 0
[828.791] !!!! Submitting initial task 2 !!!!
[828.791] Executor: Submitting new task
[828.793] Registered task f9645a1a-ea18-4bdd-b84e-803cef13a177, total tasks: 3
[828.794] Executor: Adding task f9645a1a-ea18-4bdd-b84e-803cef13a177 to server 0 (has 3 workers)
[828.795] Server 0: Adding new task
[828.795] Creating new TaskEntry with ID: 963ae328-86cb-4a45-989d-274bcfecd645
[828.796] TaskQueue: Enqueueing task 963ae328-86cb-4a45-989d-274bcfecd645
[828.796] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[828.798] UMCG syscall result: 0
[828.799] Server 0: Task queued
[828.799] Server 0: Processing event 1 for worker 6
[828.799] Task f9645a1a-ea18-4bdd-b84e-803cef13a177 assigned to server 0
[828.799] Server 0: Worker 6 blocked
[828.800] !!!! Submitting initial task 3 !!!!
[828.800] Executor: Submitting new task
[828.802] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(6044cc5a-b819-491f-a4f6-008c27c159dc))
[828.804] Server 0: Successfully switched to worker 6
[828.804] Registered task 7621d412-27b5-40a7-a215-72ffa5fba569, total tasks: 4
[828.804] Executor: Adding task 7621d412-27b5-40a7-a215-72ffa5fba569 to server 0 (has 3 workers)
[828.805] Server 0: Adding new task
[828.805] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[828.805] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.806] Creating new TaskEntry with ID: f5418895-e681-4c97-8c61-3202e73af070
[828.806] TaskQueue: Enqueueing task f5418895-e681-4c97-8c61-3202e73af070
[828.806] UMCG syscall result: 0
[828.807] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[828.807] Server 0: Task queued
[828.807] Task 7621d412-27b5-40a7-a215-72ffa5fba569 assigned to server 0
[828.807] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[828.808] !!!! Submitting initial task 4 !!!!
[828.808] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[828.809] Executor: Submitting new task
[828.810] Registered task 687c1b06-a5ba-4a3b-86b3-56839b4ac037, total tasks: 5
[828.811] Executor: Adding task 687c1b06-a5ba-4a3b-86b3-56839b4ac037 to server 0 (has 3 workers)
[828.809] Server 0: Processing event 2 for worker 6
[828.811] Server 0: Adding new task
[828.811] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[828.811] Creating new TaskEntry with ID: 43b6ad84-74ee-4b7a-ad71-c73d3edf4d31
[828.812] TaskQueue: Enqueueing task 43b6ad84-74ee-4b7a-ad71-c73d3edf4d31
[828.812] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[828.813] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[828.814] Server 0: Task queued
[828.815] Task 687c1b06-a5ba-4a3b-86b3-56839b4ac037 assigned to server 0
[828.816] !!!! Submitting initial task 5 !!!!
[828.816] Executor: Submitting new task
[828.817] Registered task f83b16dd-2498-46a1-b6a9-62407f51f650, total tasks: 6
[828.817] Executor: Adding task f83b16dd-2498-46a1-b6a9-62407f51f650 to server 0 (has 3 workers)
[828.819] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[828.820] Server 0: Adding new task
[828.821] Server 0: Worker 6 unblocking
[828.821] WorkerPool: Updating worker 2 status from Blocked to Running
[828.821] Creating new TaskEntry with ID: e2dbaf9c-e1a3-4654-a1e0-83965825448d
[828.822] TaskQueue: Enqueueing task e2dbaf9c-e1a3-4654-a1e0-83965825448d
[828.823] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[828.821] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[828.825] Server 0: Context switching to worker 6
[828.826] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[828.827] Server 0: Task queued
[828.827] Task f83b16dd-2498-46a1-b6a9-62407f51f650 assigned to server 0
[828.795] UMCG syscall result: 0
[828.828] Worker 2: UMCG registration complete with server 0
[828.828] All tasks submitted, waiting for completion...
[828.830] UMCG syscall result: 0
[828.830] Server 0: Processing event 1 for worker 6
[828.831] Server 0: Worker 6 blocked
[828.833] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(6044cc5a-b819-491f-a4f6-008c27c159dc))
[828.834] Progress: 0/6 tasks completed
[828.834] Server 0: Attempting to schedule tasks...
[828.835] Server 0: Looking for waiting workers among 3 workers
[828.836] Server 0: Found waiting worker 0 with status Waiting
[828.836] TaskQueue: Attempting to get next task
[828.837] TaskQueue: Retrieved task f2042d95-6856-4ced-b37a-12956a90cb9a
[828.837] Server 0: Assigning task to worker 0
[828.837] WorkerPool: Updating worker 0 status from Waiting to Running
[828.838] TaskQueue: Marking task f2042d95-6856-4ced-b37a-12956a90cb9a as in progress with worker 4
[828.839] Worker 0: Starting task assignment
[828.839] Worker 0: Task sent successfully
[828.839] Server 0: Context switching to worker 4
[828.839] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[828.840] UMCG syscall result: 0
[828.840] Worker 0: UMCG registration complete with server 0
[828.840] Worker 0: Entering task processing loop
[828.842] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[828.842] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[828.843] !!!! Initial task 1: STARTING task !!!!
[828.843] !!!! Initial task 1: ABOUT TO SLEEP !!!!
[828.844] UMCG syscall result: 0
[828.844] Server 0: Processing event 1 for worker 4
[828.844] Server 0: Worker 4 blocked
[828.845] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(f2042d95-6856-4ced-b37a-12956a90cb9a))
[828.846] Server 0: Successfully switched to worker 4
[828.847] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[828.847] Server 0: Attempting to schedule tasks...
[828.847] Server 0: Looking for waiting workers among 3 workers
[828.847] Server 0: Found waiting worker 1 with status Waiting
[828.848] TaskQueue: Attempting to get next task
[828.848] TaskQueue: Retrieved task 963ae328-86cb-4a45-989d-274bcfecd645
[828.849] Server 0: Assigning task to worker 1
[828.849] WorkerPool: Updating worker 1 status from Waiting to Running
[828.850] TaskQueue: Marking task 963ae328-86cb-4a45-989d-274bcfecd645 as in progress with worker 5
[828.850] Worker 1: Starting task assignment
[828.852] Worker 1: Task sent successfully
[828.852] Server 0: Context switching to worker 5
[828.852] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[828.852] UMCG syscall result: 0
[828.852] Worker 1: UMCG registration complete with server 0
[828.853] Worker 1: Entering task processing loop
[828.854] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[828.854] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[828.854] !!!! Initial task 2: STARTING task !!!!
[828.854] !!!! Initial task 2: ABOUT TO SLEEP !!!!
[828.855] UMCG syscall result: 0
[828.855] Server 0: Processing event 1 for worker 5
[828.855] Server 0: Worker 5 blocked
[828.856] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(963ae328-86cb-4a45-989d-274bcfecd645))
[828.856] Server 0: Successfully switched to worker 5
[828.857] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[828.857] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.858] UMCG syscall result: 0
[828.858] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[828.858] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[828.859] Server 0: Processing event 2 for worker 6
[828.859] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[828.859] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[828.860] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[828.860] Server 0: Worker 6 unblocking
[828.860] WorkerPool: Updating worker 2 status from Blocked to Running
[828.861] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[828.861] Server 0: Context switching to worker 6
[828.861] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[828.829] Worker 2: Entering task processing loop
[828.862] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[828.862] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[828.862] !!!! Initial task 0: STARTING task !!!!
[828.863] !!!! Initial task 0: ABOUT TO SLEEP !!!!
[828.863] UMCG syscall result: 0
[828.864] Server 0: Processing event 1 for worker 6
[828.864] Server 0: Worker 6 blocked
[828.864] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(6044cc5a-b819-491f-a4f6-008c27c159dc))
[828.865] Server 0: Attempting to schedule tasks...
[828.865] Server 0: Looking for waiting workers among 3 workers
[828.866] Server 0: No waiting workers found
[828.866] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[828.866] Server 0: Attempting to schedule tasks...
[828.866] Server 0: Looking for waiting workers among 3 workers
[828.867] Server 0: No waiting workers found
[828.867] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[828.868] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[828.938] Progress: 0/6 tasks completed
[829.045] Progress: 0/6 tasks completed
[829.153] Progress: 0/6 tasks completed
[829.278] Progress: 0/6 tasks completed
[829.389] Progress: 0/6 tasks completed
[829.501] Progress: 0/6 tasks completed
[829.611] Progress: 0/6 tasks completed
[829.722] Progress: 0/6 tasks completed
[829.827] Progress: 0/6 tasks completed
en1: assigned FE80::9839:3FFF:FEB7:323
[830.863] UMCG syscall result: 0
[830.864] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[830.864] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[830.865] Server 0: Processing event 2 for worker 4
[830.867] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.867] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[830.868] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.868] Server 0: Worker 4 unblocking
[830.869] WorkerPool: Updating worker 0 status from Blocked to Running
[830.870] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[830.870] Server 0: Context switching to worker 4
[830.871] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[830.872] !!!! Initial task 1: WOKE UP FROM SLEEP !!!!
[830.873] !!!! Initial task 1: PREPARING to spawn child task !!!!
[830.873] TaskHandle: Submitting new task
[830.874] Executor: Submitting new task
[830.876] Registered task 27e20d31-4250-4fc2-bdb5-8eed8e919b68, total tasks: 7
[830.878] Executor: Adding task 27e20d31-4250-4fc2-bdb5-8eed8e919b68 to server 0 (has 3 workers)
[830.878] Server 0: Adding new task
[830.879] Creating new TaskEntry with ID: cdb094e9-cf00-4d3b-80ed-f876aad2b5fd
[830.879] TaskQueue: Enqueueing task cdb094e9-cf00-4d3b-80ed-f876aad2b5fd
[830.881] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[830.881] Server 0: Task queued
[830.882] Task 27e20d31-4250-4fc2-bdb5-8eed8e919b68 assigned to server 0
[830.883] UMCG syscall result: 0
[830.883] Server 0: Processing event 1 for worker 4
[830.884] Server 0: Worker 4 blocked
[830.884] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(f2042d95-6856-4ced-b37a-12956a90cb9a))
[830.885] Server 0: Attempting to schedule tasks...
[830.885] Server 0: Looking for waiting workers among 3 workers
[830.885] Server 0: No waiting workers found
[830.886] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[830.886] Server 0: Attempting to schedule tasks...
[830.887] Server 0: Looking for waiting workers among 3 workers
[830.887] Server 0: No waiting workers found
[830.887] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[830.888] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.888] UMCG syscall result: 0
[830.889] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[830.889] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[830.890] Server 0: Processing event 2 for worker 6
[830.890] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.890] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[830.890] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.891] Server 0: Worker 6 unblocking
[830.891] WorkerPool: Updating worker 2 status from Blocked to Running
[830.892] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[830.892] Server 0: Context switching to worker 6
[830.892] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[830.894] !!!! Initial task 0: WOKE UP FROM SLEEP !!!!
[830.894] !!!! Initial task 0: PREPARING to spawn child task !!!!
[830.895] TaskHandle: Submitting new task
[830.895] Executor: Submitting new task
[830.897] Registered task ae99e671-6818-4cab-8e13-4951d38ab255, total tasks: 8
[830.898] Executor: Adding task ae99e671-6818-4cab-8e13-4951d38ab255 to server 0 (has 3 workers)
[830.898] Server 0: Adding new task
[830.898] Creating new TaskEntry with ID: 28f73246-9101-474f-ad26-b918057186ae
[830.899] TaskQueue: Enqueueing task 28f73246-9101-474f-ad26-b918057186ae
[830.900] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[830.900] Server 0: Task queued
[830.900] Task ae99e671-6818-4cab-8e13-4951d38ab255 assigned to server 0
[830.900] UMCG syscall result: 0
[830.901] Server 0: Processing event 1 for worker 6
[830.901] Server 0: Worker 6 blocked
[830.901] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(6044cc5a-b819-491f-a4f6-008c27c159dc))
[830.902] Server 0: Attempting to schedule tasks...
[830.902] Server 0: Looking for waiting workers among 3 workers
[830.902] Server 0: No waiting workers found
[830.903] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[830.903] Server 0: Processing event 2 for worker 5
[830.903] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.903] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[830.904] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.905] Server 0: Worker 5 unblocking
[830.905] WorkerPool: Updating worker 1 status from Blocked to Running
[830.905] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[830.906] Server 0: Context switching to worker 5
[830.906] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[830.907] !!!! Initial task 2: WOKE UP FROM SLEEP !!!!
[830.907] !!!! Initial task 2: PREPARING to spawn child task !!!!
[830.908] TaskHandle: Submitting new task
[830.908] Executor: Submitting new task
[830.909] Registered task 32b3e798-989c-4e72-b1bc-cf6280802dc1, total tasks: 9
[830.909] Executor: Adding task 32b3e798-989c-4e72-b1bc-cf6280802dc1 to server 0 (has 3 workers)
[830.910] Server 0: Adding new task
[830.910] Creating new TaskEntry with ID: 10dd4f14-06c1-4dc9-8243-850a0f08d1b5
[830.910] TaskQueue: Enqueueing task 10dd4f14-06c1-4dc9-8243-850a0f08d1b5
[830.911] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[830.911] Server 0: Task queued
[830.912] Task 32b3e798-989c-4e72-b1bc-cf6280802dc1 assigned to server 0
[830.912] UMCG syscall result: 0
[830.912] Server 0: Processing event 1 for worker 5
[830.912] Server 0: Worker 5 blocked
[830.913] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(963ae328-86cb-4a45-989d-274bcfecd645))
[830.913] Server 0: Attempting to schedule tasks...
[830.913] Server 0: Looking for waiting workers among 3 workers
[830.914] Server 0: No waiting workers found
[830.914] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[830.915] Server 0: Attempting to schedule tasks...
[830.915] Server 0: Looking for waiting workers among 3 workers
[830.915] Server 0: No waiting workers found
[830.916] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[830.916] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.917] UMCG syscall result: 0
[830.917] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[830.918] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[830.918] Server 0: Processing event 2 for worker 4
[830.918] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.919] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[830.919] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.919] Server 0: Worker 4 unblocking
[830.919] WorkerPool: Updating worker 0 status from Blocked to Running
[830.920] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[830.920] Server 0: Context switching to worker 4
[830.920] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[830.921] TaskHandle: Task submitted successfully
[830.921] !!!! Initial task 1: COMPLETED !!!!
[830.922] Completed task 9d99075f-5df6-48f2-8b45-856842eb009f, total completed: 1/9
[830.923] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[830.924] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[830.924] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.925] UMCG syscall result: 0
[830.925] Server 0: Processing event 3 for worker 4
[830.925] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[830.926] Server 0: Got explicit WAIT from worker 4
[830.926] Server 0: Worker 4 current status: Running
[830.926] WorkerPool: Updating worker 0 status from Running to Waiting
[830.927] Server 0: Attempting to schedule tasks...
[830.927] Server 0: Looking for waiting workers among 3 workers
[830.927] Server 0: Found waiting worker 0 with status Waiting
[830.928] TaskQueue: Attempting to get next task
[830.928] TaskQueue: Retrieved task f5418895-e681-4c97-8c61-3202e73af070
[830.928] Server 0: Assigning task to worker 0
[830.928] WorkerPool: Updating worker 0 status from Waiting to Running
[830.929] TaskQueue: Marking task f5418895-e681-4c97-8c61-3202e73af070 as in progress with worker 4
[830.929] Worker 0: Starting task assignment
[830.929] Worker 0: Task sent successfully
[830.929] Server 0: Context switching to worker 4
[830.930] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[830.930] UMCG syscall result: 0
[830.930] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[830.931] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[830.931] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[830.931] !!!! Initial task 3: STARTING task !!!!
[830.931] !!!! Initial task 3: ABOUT TO SLEEP !!!!
[830.932] UMCG syscall result: 0
[830.932] Server 0: Processing event 1 for worker 4
[830.933] Server 0: Worker 4 blocked
[830.933] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(f5418895-e681-4c97-8c61-3202e73af070))
[830.933] Server 0: Successfully switched to worker 4
[830.934] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[830.934] Server 0: Processing event 2 for worker 6
[830.934] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.934] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[830.935] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.935] Server 0: Worker 6 unblocking
[830.936] WorkerPool: Updating worker 2 status from Blocked to Running
[830.936] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[830.936] Server 0: Context switching to worker 6
[830.937] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[830.937] TaskHandle: Task submitted successfully
[830.937] !!!! Initial task 0: COMPLETED !!!!
[830.938] Completed task ae82ede3-6607-4cc2-8dac-ec5acc05f1a3, total completed: 2/9
[830.939] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[830.939] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[830.939] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.940] UMCG syscall result: 0
[830.940] Server 0: Processing event 3 for worker 6
[830.940] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[830.940] Server 0: Got explicit WAIT from worker 6
[830.941] Server 0: Worker 6 current status: Running
[830.941] WorkerPool: Updating worker 2 status from Running to Waiting
[830.941] Server 0: Attempting to schedule tasks...
[830.941] Server 0: Looking for waiting workers among 3 workers
[830.942] Server 0: Found waiting worker 2 with status Waiting
[830.942] TaskQueue: Attempting to get next task
[830.943] TaskQueue: Retrieved task 43b6ad84-74ee-4b7a-ad71-c73d3edf4d31
[830.943] Server 0: Assigning task to worker 2
[830.943] WorkerPool: Updating worker 2 status from Waiting to Running
[830.944] TaskQueue: Marking task 43b6ad84-74ee-4b7a-ad71-c73d3edf4d31 as in progress with worker 6
[830.944] Worker 2: Starting task assignment
[830.944] Worker 2: Task sent successfully
[830.945] Server 0: Context switching to worker 6
[830.945] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[830.945] UMCG syscall result: 0
[830.945] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[830.945] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[830.946] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[830.946] !!!! Initial task 4: STARTING task !!!!
[830.946] !!!! Initial task 4: ABOUT TO SLEEP !!!!
[830.947] UMCG syscall result: 0
[830.947] Server 0: Processing event 1 for worker 6
[830.947] Server 0: Worker 6 blocked
[830.947] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(43b6ad84-74ee-4b7a-ad71-c73d3edf4d31))
[830.948] Server 0: Successfully switched to worker 6
[830.948] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[830.949] Server 0: Attempting to schedule tasks...
[830.949] Server 0: Looking for waiting workers among 3 workers
[830.949] Server 0: No waiting workers found
[830.949] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[830.950] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.950] UMCG syscall result: 0
[830.950] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[830.950] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[830.951] Server 0: Processing event 2 for worker 5
[830.951] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[830.951] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[830.952] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[830.952] Server 0: Worker 5 unblocking
[830.952] WorkerPool: Updating worker 1 status from Blocked to Running
[830.953] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[830.953] Server 0: Context switching to worker 5
[830.953] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[830.953] TaskHandle: Task submitted successfully
[830.953] !!!! Initial task 2: COMPLETED !!!!
[830.954] Completed task f9645a1a-ea18-4bdd-b84e-803cef13a177, total completed: 3/9
[830.954] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[830.954] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[830.955] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[830.955] UMCG syscall result: 0
[830.955] Server 0: Processing event 3 for worker 5
[830.955] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[830.956] Server 0: Got explicit WAIT from worker 5
[830.956] Server 0: Worker 5 current status: Running
[830.956] WorkerPool: Updating worker 1 status from Running to Waiting
[830.957] Server 0: Attempting to schedule tasks...
[830.957] Server 0: Looking for waiting workers among 3 workers
[830.957] Server 0: Found waiting worker 1 with status Waiting
[830.957] TaskQueue: Attempting to get next task
[830.958] TaskQueue: Retrieved task e2dbaf9c-e1a3-4654-a1e0-83965825448d
[830.958] Server 0: Assigning task to worker 1
[830.959] WorkerPool: Updating worker 1 status from Waiting to Running
[830.959] TaskQueue: Marking task e2dbaf9c-e1a3-4654-a1e0-83965825448d as in progress with worker 5
[830.959] Worker 1: Starting task assignment
[830.960] Worker 1: Task sent successfully
[830.960] Server 0: Context switching to worker 5
[830.960] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[830.960] UMCG syscall result: 0
[830.961] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[830.961] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[830.961] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[830.962] !!!! Initial task 5: STARTING task !!!!
[830.962] !!!! Initial task 5: ABOUT TO SLEEP !!!!
[830.962] UMCG syscall result: 0
[830.962] Server 0: Processing event 1 for worker 5
[830.963] Server 0: Worker 5 blocked
[830.963] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(e2dbaf9c-e1a3-4654-a1e0-83965825448d))
[830.964] Server 0: Successfully switched to worker 5
[830.964] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[830.964] Server 0: Attempting to schedule tasks...
[830.965] Server 0: Looking for waiting workers among 3 workers
[830.965] Server 0: No waiting workers found
[830.965] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[830.966] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[832.939] UMCG syscall result: 0
[832.940] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[832.941] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[832.941] Server 0: Processing event 2 for worker 4
[832.942] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[832.944] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[832.945] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[832.946] Server 0: Worker 4 unblocking
[832.946] WorkerPool: Updating worker 0 status from Blocked to Running
[832.947] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[832.948] Server 0: Context switching to worker 4
[832.948] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[832.949] !!!! Initial task 3: WOKE UP FROM SLEEP !!!!
[832.950] !!!! Initial task 3: PREPARING to spawn child task !!!!
[832.950] TaskHandle: Submitting new task
[832.950] Executor: Submitting new task
[832.952] Registered task cccffd9b-f7cc-4943-91c8-36467e2eebdc, total tasks: 10
[832.953] Executor: Adding task cccffd9b-f7cc-4943-91c8-36467e2eebdc to server 0 (has 3 workers)
[832.953] Server 0: Adding new task
[832.953] Creating new TaskEntry with ID: 78c0a495-5ffe-4491-aeef-d3495036e3c7
[832.955] TaskQueue: Enqueueing task 78c0a495-5ffe-4491-aeef-d3495036e3c7
[832.955] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[832.956] Server 0: Task queued
[832.956] Task cccffd9b-f7cc-4943-91c8-36467e2eebdc assigned to server 0
[832.956] UMCG syscall result: 0
[832.957] Server 0: Processing event 1 for worker 4
[832.957] Server 0: Worker 4 blocked
[832.958] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(f5418895-e681-4c97-8c61-3202e73af070))
[832.959] Server 0: Attempting to schedule tasks...
[832.959] Server 0: Looking for waiting workers among 3 workers
[832.959] Server 0: No waiting workers found
[832.959] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[832.960] Server 0: Attempting to schedule tasks...
[832.960] Server 0: Looking for waiting workers among 3 workers
[832.961] Server 0: No waiting workers found
[832.961] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[832.962] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[832.962] UMCG syscall result: 0
[832.963] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[832.963] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[832.964] Server 0: Processing event 2 for worker 6
[832.964] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[832.965] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[832.965] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[832.966] Server 0: Worker 6 unblocking
[832.966] WorkerPool: Updating worker 2 status from Blocked to Running
[832.967] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[832.967] Server 0: Context switching to worker 6
[832.968] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[832.969] !!!! Initial task 4: WOKE UP FROM SLEEP !!!!
[832.969] !!!! Initial task 4: PREPARING to spawn child task !!!!
[832.970] TaskHandle: Submitting new task
[832.970] Executor: Submitting new task
[832.970] Registered task 56d8723b-9c40-4aea-af0c-fb761eac43ee, total tasks: 11
[832.970] Executor: Adding task 56d8723b-9c40-4aea-af0c-fb761eac43ee to server 0 (has 3 workers)
[832.971] Server 0: Adding new task
[832.971] Creating new TaskEntry with ID: 29000f29-4047-4383-ab21-6e4e656efc23
[832.972] TaskQueue: Enqueueing task 29000f29-4047-4383-ab21-6e4e656efc23
[832.972] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[832.972] Server 0: Task queued
[832.973] Task 56d8723b-9c40-4aea-af0c-fb761eac43ee assigned to server 0
[832.973] UMCG syscall result: 0
[832.973] Server 0: Processing event 1 for worker 6
[832.974] Server 0: Worker 6 blocked
[832.974] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(43b6ad84-74ee-4b7a-ad71-c73d3edf4d31))
[832.975] Server 0: Attempting to schedule tasks...
[832.975] Server 0: Looking for waiting workers among 3 workers
[832.976] Server 0: No waiting workers found
[832.976] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[832.977] Server 0: Attempting to schedule tasks...
[832.977] Server 0: Looking for waiting workers among 3 workers
[832.977] Server 0: No waiting workers found
[832.978] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[832.978] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[832.978] UMCG syscall result: 0
[832.979] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[832.979] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[832.980] Server 0: Processing event 2 for worker 5
[832.980] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[832.981] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[832.983] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[832.983] Server 0: Worker 5 unblocking
[832.983] WorkerPool: Updating worker 1 status from Blocked to Running
[832.984] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[832.984] Server 0: Context switching to worker 5
[832.985] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[832.985] !!!! Initial task 5: WOKE UP FROM SLEEP !!!!
[832.986] !!!! Initial task 5: PREPARING to spawn child task !!!!
[832.986] TaskHandle: Submitting new task
[832.986] Executor: Submitting new task
[832.987] Registered task 793eec83-600c-40d6-a869-dfb60eb26558, total tasks: 12
[832.987] Executor: Adding task 793eec83-600c-40d6-a869-dfb60eb26558 to server 0 (has 3 workers)
[832.987] Server 0: Adding new task
[832.988] Creating new TaskEntry with ID: c25d92c8-d6a9-46da-9540-666867478cb4
[832.988] TaskQueue: Enqueueing task c25d92c8-d6a9-46da-9540-666867478cb4
[832.988] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[832.989] Server 0: Task queued
[832.989] Task 793eec83-600c-40d6-a869-dfb60eb26558 assigned to server 0
[832.990] UMCG syscall result: 0
[832.990] Server 0: Processing event 1 for worker 5
[832.991] Server 0: Worker 5 blocked
[832.991] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(e2dbaf9c-e1a3-4654-a1e0-83965825448d))
[832.991] Server 0: Attempting to schedule tasks...
[832.991] Server 0: Looking for waiting workers among 3 workers
[832.992] Server 0: No waiting workers found
[832.992] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[832.992] Server 0: Processing event 2 for worker 4
[832.993] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[832.993] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[832.993] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[832.993] Server 0: Worker 4 unblocking
[832.994] WorkerPool: Updating worker 0 status from Blocked to Running
[832.994] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[832.994] Server 0: Context switching to worker 4
[832.995] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[832.995] TaskHandle: Task submitted successfully
[832.995] !!!! Initial task 3: COMPLETED !!!!
[832.995] Completed task 7621d412-27b5-40a7-a215-72ffa5fba569, total completed: 4/12
[832.996] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[832.996] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[832.997] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[832.997] UMCG syscall result: 0
[832.997] Server 0: Processing event 3 for worker 4
[832.998] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[832.998] Server 0: Got explicit WAIT from worker 4
[832.998] Server 0: Worker 4 current status: Running
[832.998] WorkerPool: Updating worker 0 status from Running to Waiting
[832.999] Server 0: Attempting to schedule tasks...
[832.999] Server 0: Looking for waiting workers among 3 workers
[832.999] Server 0: Found waiting worker 0 with status Waiting
[833.000] TaskQueue: Attempting to get next task
[833.001] TaskQueue: Retrieved task cdb094e9-cf00-4d3b-80ed-f876aad2b5fd
[833.001] Server 0: Assigning task to worker 0
[833.002] WorkerPool: Updating worker 0 status from Waiting to Running
[833.002] TaskQueue: Marking task cdb094e9-cf00-4d3b-80ed-f876aad2b5fd as in progress with worker 4
[833.003] Worker 0: Starting task assignment
[833.003] Worker 0: Task sent successfully
[833.003] Server 0: Context switching to worker 4
[833.003] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[833.004] UMCG syscall result: 0
[833.004] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[833.005] !!!!!!!!!! WORKER 0: Received task from channel !!!!!!!!!!!
[833.005] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[833.006] !!!! Child task of initial task 1: STARTING work !!!!
[833.006] UMCG syscall result: 0
[833.006] Server 0: Processing event 1 for worker 4
[833.006] Server 0: Worker 4 blocked
[833.007] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(cdb094e9-cf00-4d3b-80ed-f876aad2b5fd))
[833.007] Server 0: Successfully switched to worker 4
[833.007] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[833.007] Server 0: Attempting to schedule tasks...
[833.008] Server 0: Looking for waiting workers among 3 workers
[833.008] Server 0: No waiting workers found
[833.009] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[833.009] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[833.009] UMCG syscall result: 0
[833.010] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[833.010] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[833.010] Server 0: Processing event 2 for worker 6
[833.010] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[833.011] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[833.011] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[833.012] Server 0: Worker 6 unblocking
[833.012] WorkerPool: Updating worker 2 status from Blocked to Running
[833.012] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[833.012] Server 0: Context switching to worker 6
[833.013] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[833.014] TaskHandle: Task submitted successfully
[833.014] !!!! Initial task 4: COMPLETED !!!!
[833.014] Completed task 687c1b06-a5ba-4a3b-86b3-56839b4ac037, total completed: 5/12
[833.015] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[833.015] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[833.016] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[833.016] UMCG syscall result: 0
[833.016] Server 0: Processing event 3 for worker 6
[833.016] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[833.017] Server 0: Got explicit WAIT from worker 6
[833.017] Server 0: Worker 6 current status: Running
[833.017] WorkerPool: Updating worker 2 status from Running to Waiting
[833.018] Server 0: Attempting to schedule tasks...
[833.018] Server 0: Looking for waiting workers among 3 workers
[833.018] Server 0: Found waiting worker 2 with status Waiting
[833.018] TaskQueue: Attempting to get next task
[833.018] TaskQueue: Retrieved task 28f73246-9101-474f-ad26-b918057186ae
[833.019] Server 0: Assigning task to worker 2
[833.019] WorkerPool: Updating worker 2 status from Waiting to Running
[833.019] TaskQueue: Marking task 28f73246-9101-474f-ad26-b918057186ae as in progress with worker 6
[833.019] Worker 2: Starting task assignment
[833.019] Worker 2: Task sent successfully
[833.020] Server 0: Context switching to worker 6
[833.020] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[833.020] UMCG syscall result: 0
[833.020] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[833.020] !!!!!!!!!! WORKER 2: Received task from channel !!!!!!!!!!!
[833.022] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[833.022] !!!! Child task of initial task 0: STARTING work !!!!
[833.023] UMCG syscall result: 0
[833.023] Server 0: Processing event 1 for worker 6
[833.023] Server 0: Worker 6 blocked
[833.024] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(28f73246-9101-474f-ad26-b918057186ae))
[833.024] Server 0: Successfully switched to worker 6
[833.025] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[833.026] Server 0: Processing event 2 for worker 5
[833.026] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[833.026] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[833.027] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[833.027] Server 0: Worker 5 unblocking
[833.027] WorkerPool: Updating worker 1 status from Blocked to Running
[833.027] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[833.028] Server 0: Context switching to worker 5
[833.028] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[833.028] TaskHandle: Task submitted successfully
[833.028] !!!! Initial task 5: COMPLETED !!!!
[833.028] Completed task f83b16dd-2498-46a1-b6a9-62407f51f650, total completed: 6/12
[833.029] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[833.029] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[833.029] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[833.029] UMCG syscall result: 0
[833.030] Server 0: Processing event 3 for worker 5
[833.030] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[833.030] Server 0: Got explicit WAIT from worker 5
[833.030] Server 0: Worker 5 current status: Running
[833.031] WorkerPool: Updating worker 1 status from Running to Waiting
[833.031] Server 0: Attempting to schedule tasks...
[833.031] Server 0: Looking for waiting workers among 3 workers
[833.031] Server 0: Found waiting worker 1 with status Waiting
[833.031] TaskQueue: Attempting to get next task
[833.032] TaskQueue: Retrieved task 10dd4f14-06c1-4dc9-8243-850a0f08d1b5
[833.032] Server 0: Assigning task to worker 1
[833.033] WorkerPool: Updating worker 1 status from Waiting to Running
[833.033] TaskQueue: Marking task 10dd4f14-06c1-4dc9-8243-850a0f08d1b5 as in progress with worker 5
[833.033] Worker 1: Starting task assignment
[833.033] Worker 1: Task sent successfully
[833.034] Server 0: Context switching to worker 5
[833.034] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[833.034] UMCG syscall result: 0
[833.034] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[833.034] !!!!!!!!!! WORKER 1: Received task from channel !!!!!!!!!!!
[833.035] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[833.035] !!!! Child task of initial task 2: STARTING work !!!!
[833.035] UMCG syscall result: 0
[833.036] Server 0: Processing event 1 for worker 5
[833.036] Server 0: Worker 5 blocked
[833.036] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(10dd4f14-06c1-4dc9-8243-850a0f08d1b5))
[833.037] Server 0: Successfully switched to worker 5
[833.037] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[833.038] Server 0: Attempting to schedule tasks...
[833.038] Server 0: Looking for waiting workers among 3 workers
[833.038] Server 0: No waiting workers found
[833.039] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[833.039] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[833.832] Progress: 6/12 tasks completed
[833.937] Progress: 6/12 tasks completed
[834.015] UMCG syscall result: 0
[834.016] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[834.017] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[834.020] Server 0: Processing event 2 for worker 4
[834.020] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[834.022] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[834.023] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[834.025] Server 0: Worker 4 unblocking
[834.026] WorkerPool: Updating worker 0 status from Blocked to Running
[834.027] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[834.028] Server 0: Context switching to worker 4
[834.028] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[834.029] !!!! Child task of initial task 1: COMPLETED !!!!
[834.030] Completed task 27e20d31-4250-4fc2-bdb5-8eed8e919b68, total completed: 7/12
[834.030] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[834.031] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[834.031] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[834.032] UMCG syscall result: 0
[834.032] Server 0: Processing event 3 for worker 4
[834.033] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[834.034] Server 0: Got explicit WAIT from worker 4
[834.034] Server 0: Worker 4 current status: Running
[834.034] WorkerPool: Updating worker 0 status from Running to Waiting
[834.034] Server 0: Attempting to schedule tasks...
[834.035] Server 0: Looking for waiting workers among 3 workers
[834.035] Server 0: Found waiting worker 0 with status Waiting
[834.036] TaskQueue: Attempting to get next task
[834.036] TaskQueue: Retrieved task 78c0a495-5ffe-4491-aeef-d3495036e3c7
[834.037] Server 0: Assigning task to worker 0
[834.037] WorkerPool: Updating worker 0 status from Waiting to Running
[834.038] TaskQueue: Marking task 78c0a495-5ffe-4491-aeef-d3495036e3c7 as in progress with worker 4
[834.039] Worker 0: Starting task assignment
[834.040] Worker 0: Task sent successfully
[834.040] Server 0: Context switching to worker 4
[834.040] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[834.040] Progress: 7/12 tasks completed
[834.148] Progress: 7/12 tasks completed
[834.253] Progress: 7/12 tasks completed
[834.361] Progress: 7/12 tasks completed
[834.468] Progress: 7/12 tasks completed
[834.579] Progress: 7/12 tasks completed
[834.684] Progress: 7/12 tasks completed
[834.789] Progress: 7/12 tasks completed
[838.907] Progress: 7/12 tasks completed
[839.015] Progress: 7/12 tasks completed
[839.124] Progress: 7/12 tasks completed
[839.233] Progress: 7/12 tasks completed
[839.337] Progress: 7/12 tasks completed
[839.443] Progress: 7/12 tasks completed
[839.548] Progress: 7/12 tasks completed
[839.657] Progress: 7/12 tasks completed
[839.767] Progress: 7/12 tasks completed
[843.890] Progress: 7/12 tasks completed
[843.998] Progress: 7/12 tasks completed
[844.107] Progress: 7/12 tasks completed
[844.215] Progress: 7/12 tasks completed
[844.322] Progress: 7/12 tasks completed
[844.431] Progress: 7/12 tasks completed
[844.536] Progress: 7/12 tasks completed
[844.639] Progress: 7/12 tasks completed
[844.743] Progress: 7/12 tasks completed

// no tasks ever worked - 0/12
➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[654.568] Creating new Executor
[654.584] Creating Server 0 on CPU 0
[654.593] Creating Server 0
[654.596] Creating new TaskQueue
[654.604] Creating WorkerPool with capacity for 3 workers
[654.609] Starting executor...
[654.609] Executor: Starting servers
[654.636] Starting server 0 initialization
[654.638] Server 0 ready for tasks
[654.655] Successfully set CPU affinity to 0
[654.660] Server 0: Starting up
[654.665] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[654.668] UMCG syscall result: 0
[654.670] Server 0: UMCG registration complete
[654.672] Server 0: Initializing workers
[654.673] Server 0: Initializing worker 0
[654.672] All 1 servers started and ready
[654.678] Starting WorkerThread 0 for server 0
[654.693] Successfully set CPU affinity to 0
[654.694] Worker 0: Initialized with tid 4
[654.696] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[654.696] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[654.699] WorkerThread 0 started with tid 4 for server 0
[654.703] Creating new Worker 0 for server 0
[654.703] WorkerPool: Adding worker 0 with tid 4
[654.706] WorkerPool: Updating worker 0 status from Initializing to Registering
[654.707] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[654.708] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[654.710] UMCG syscall result: 0
[654.711] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[654.711] Worker 0: Registering worker event 130
[654.712] WorkerPool: Updating worker 0 status from Registering to Waiting
[654.713] Server 0: Worker 0 initialized successfully
[654.713] Server 0: Initializing worker 1
[654.715] Starting WorkerThread 1 for server 0
[654.723] Successfully set CPU affinity to 0
[654.723] Worker 1: Initialized with tid 5
[654.729] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[654.730] WorkerThread 1 started with tid 5 for server 0
[654.731] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[654.732] Creating new Worker 1 for server 0
[654.735] WorkerPool: Adding worker 1 with tid 5
[654.736] WorkerPool: Updating worker 1 status from Initializing to Registering
[654.734] Submitting initial tasks...
[654.736] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[654.739] !!!! Submitting initial task 0 !!!!
[654.740] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[654.742] UMCG syscall result: 0
[654.742] Executor: Submitting new task
[654.743] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[654.746] Worker 0: Registering worker event 162
[654.747] WorkerPool: Updating worker 1 status from Registering to Waiting
[654.747] Server 0: Worker 1 initialized successfully
[654.748] Server 0: Initializing worker 2
[654.750] Starting WorkerThread 2 for server 0
[654.756] Successfully set CPU affinity to 0
[654.757] Worker 2: Initialized with tid 6
[654.763] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[654.763] Registered task ad31abf5-6672-4419-aa17-18e7a3e323c6, total tasks: 1
[654.764] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[654.764] WorkerThread 2 started with tid 6 for server 0
[654.765] Creating new Worker 2 for server 0
[654.765] WorkerPool: Adding worker 2 with tid 6
[654.771] WorkerPool: Updating worker 2 status from Initializing to Registering
[654.773] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[654.773] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[654.774] UMCG syscall result: 0
[654.774] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[654.774] Worker 0: Registering worker event 194
[654.774] WorkerPool: Updating worker 2 status from Registering to Waiting
[654.775] Server 0: Worker 2 initialized successfully
[654.776] Server 0: All workers initialized
[654.779] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[654.781] Server 0: Attempting to schedule tasks...
[654.781] Server 0: Looking for waiting workers among 3 workers
[654.782] Server 0: Found waiting worker 2 with status Waiting
[654.784] TaskQueue: Attempting to get next task
[654.785] Executor: Adding task ad31abf5-6672-4419-aa17-18e7a3e323c6 to server 0 (has 3 workers)
[654.786] TaskQueue: No tasks available
[654.787] Server 0: Adding new task
[654.789] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[654.789] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[654.790] Creating new TaskEntry with ID: 3ca44fa1-a0b0-4033-bfcb-79fa79345c3f
[654.793] TaskQueue: Enqueueing task 3ca44fa1-a0b0-4033-bfcb-79fa79345c3f
[654.794] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[654.795] Server 0: Task queued
[654.796] Task ad31abf5-6672-4419-aa17-18e7a3e323c6 assigned to server 0
[654.796] !!!! Submitting initial task 1 !!!!
[654.797] Executor: Submitting new task
[654.799] Registered task b6abd2ef-8c24-4a72-98e3-4a6561e2393e, total tasks: 2
[654.800] Executor: Adding task b6abd2ef-8c24-4a72-98e3-4a6561e2393e to server 0 (has 3 workers)
[654.801] Server 0: Adding new task
[654.801] Creating new TaskEntry with ID: d696d9bc-830d-4937-bf68-70461d5ba5fe
[654.802] TaskQueue: Enqueueing task d696d9bc-830d-4937-bf68-70461d5ba5fe
[654.803] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[654.803] Server 0: Task queued
[654.803] Task b6abd2ef-8c24-4a72-98e3-4a6561e2393e assigned to server 0
[654.804] !!!! Submitting initial task 2 !!!!
[654.804] Executor: Submitting new task
[654.805] Registered task 457608d2-6ce9-4f9d-b530-f48bf1a42766, total tasks: 3
[654.805] Executor: Adding task 457608d2-6ce9-4f9d-b530-f48bf1a42766 to server 0 (has 3 workers)
[654.806] Server 0: Adding new task
[654.806] Creating new TaskEntry with ID: 3875a9af-4e73-487c-af42-ec46c52d4831
[654.806] TaskQueue: Enqueueing task 3875a9af-4e73-487c-af42-ec46c52d4831
[654.807] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[654.807] Server 0: Task queued
[654.807] Task 457608d2-6ce9-4f9d-b530-f48bf1a42766 assigned to server 0
[654.808] !!!! Submitting initial task 3 !!!!
[654.808] Executor: Submitting new task
[654.810] Registered task b9ebb3f1-b696-4ee2-9181-a4118a9f2b10, total tasks: 4
[654.810] Executor: Adding task b9ebb3f1-b696-4ee2-9181-a4118a9f2b10 to server 0 (has 3 workers)
[654.810] Server 0: Adding new task
[654.811] Creating new TaskEntry with ID: 35e755f6-ceb9-449e-a0f6-9e428db65421
[654.811] TaskQueue: Enqueueing task 35e755f6-ceb9-449e-a0f6-9e428db65421
[654.812] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[654.812] Server 0: Task queued
[654.812] Task b9ebb3f1-b696-4ee2-9181-a4118a9f2b10 assigned to server 0
[654.813] !!!! Submitting initial task 4 !!!!
[654.813] Executor: Submitting new task
[654.813] Registered task 43c228cc-1778-47d7-81d9-36d0d3687fc8, total tasks: 5
[654.814] Executor: Adding task 43c228cc-1778-47d7-81d9-36d0d3687fc8 to server 0 (has 3 workers)
[654.814] Server 0: Adding new task
[654.814] Creating new TaskEntry with ID: b3a61471-5829-48af-8183-b19b74e03729
[654.815] TaskQueue: Enqueueing task b3a61471-5829-48af-8183-b19b74e03729
[654.815] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[654.816] Server 0: Task queued
[654.816] Task 43c228cc-1778-47d7-81d9-36d0d3687fc8 assigned to server 0
[654.816] !!!! Submitting initial task 5 !!!!
[654.816] Executor: Submitting new task
[654.817] Registered task 8de93f5d-e453-4687-a8e8-fdca4d56bcd8, total tasks: 6
[654.817] Executor: Adding task 8de93f5d-e453-4687-a8e8-fdca4d56bcd8 to server 0 (has 3 workers)
[654.817] Server 0: Adding new task
[654.818] Creating new TaskEntry with ID: b2c00535-7e25-4e2c-8bc6-0b06bf2a0f36
[654.819] TaskQueue: Enqueueing task b2c00535-7e25-4e2c-8bc6-0b06bf2a0f36
[654.820] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[654.820] Server 0: Task queued
[654.820] Task 8de93f5d-e453-4687-a8e8-fdca4d56bcd8 assigned to server 0
[654.821] All tasks submitted, waiting for completion...
[654.823] Progress: 0/6 tasks completed
[654.934] Progress: 0/6 tasks completed
[655.056] Progress: 0/6 tasks completed
[655.165] Progress: 0/6 tasks completed
[655.281] Progress: 0/6 tasks completed
[655.391] Progress: 0/6 tasks completed
[655.500] Progress: 0/6 tasks completed
[655.606] Progress: 0/6 tasks completed
[655.715] Progress: 0/6 tasks completed
[655.821] Progress: 0/6 tasks completed
en1: assigned FE80::B044:F4FF:FEE3:4D1A
[659.826] Progress: 0/6 tasks completed
[659.930] Progress: 0/6 tasks completed
[660.033] Progress: 0/6 tasks completed
[660.137] Progress: 0/6 tasks completed
[660.241] Progress: 0/6 tasks completed
[660.347] Progress: 0/6 tasks completed
[660.454] Progress: 0/6 tasks completed
[660.559] Progress: 0/6 tasks completed
[660.669] Progress: 0/6 tasks completed
[660.773] Progress: 0/6 tasks completed
[664.882] Progress: 0/6 tasks completed
[664.992] Progress: 0/6 tasks completed
[665.102] Progress: 0/6 tasks completed
[665.208] Progress: 0/6 tasks completed
[665.312] Progress: 0/6 tasks completed
[665.419] Progress: 0/6 tasks completed
[665.526] Progress: 0/6 tasks completed
[665.630] Progress: 0/6 tasks completed
[665.732] Progress: 0/6 tasks completed
[669.849] Progress: 0/6 tasks completed
[669.958] Progress: 0/6 tasks completed
[670.064] Progress: 0/6 tasks completed
[670.174] Progress: 0/6 tasks completed
[670.278] Progress: 0/6 tasks completed
[670.383] Progress: 0/6 tasks completed
[670.489] Progress: 0/6 tasks completed
[670.599] Progress: 0/6 tasks completed
[670.709] Progress: 0/6 tasks completed
[670.817] Progress: 0/6 tasks completed
[674.829] Progress: 0/6 tasks completed
[674.933] Progress: 0/6 tasks completed
[675.039] Progress: 0/6 tasks completed
[675.145] Progress: 0/6 tasks completed
[675.251] Progress: 0/6 tasks completed
[675.361] Progress: 0/6 tasks completed
[675.466] Progress: 0/6 tasks completed
[675.574] Progress: 0/6 tasks completed
[675.683] Progress: 0/6 tasks completed
[675.787] Progress: 0/6 tasks completed
[679.916] Progress: 0/6 tasks completed
[680.030] Progress: 0/6 tasks completed
[680.132] Progress: 0/6 tasks completed
[680.235] Progress: 0/6 tasks completed
[680.340] Progress: 0/6 tasks completed
[680.446] Progress: 0/6 tasks completed
[680.556] Progress: 0/6 tasks completed
[680.661] Progress: 0/6 tasks completed
[680.771] Progress: 0/6 tasks completed
[684.885] Timeout waiting for tasks to complete! (0/6 completed)
[684.887] Initiating executor shutdown...
[684.888] Server 0: Adding new task
[684.891] Server 0: Received shutdown signal*/


/*
Ignore the code below this it was for a simple test
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