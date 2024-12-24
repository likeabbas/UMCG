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
}

impl Worker {
    fn assign_task(&self, mut task: TaskEntry, tx: &Sender<Task>) -> Result<(), TaskEntry> {
        info!("Worker {}: Starting task assignment", self.id);

        // Extract the task_fn, but only send it if the channel is available
        match &task.state {
            TaskState::Pending(_) => {
                // Extract and send the actual task
                if let TaskState::Pending(task_fn) = std::mem::replace(&mut task.state, TaskState::Completed) {
                    match tx.send(Task::Function(task_fn)) {
                        Ok(_) => {
                            debug!("Worker {}: Task sent successfully", self.id);
                            Ok(())
                        },
                        Err(_) => {
                            debug!("Worker {}: Channel is disconnected", self.id);
                            Err(task)  // Return original task untouched
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
    available_workers: Arc<Mutex<HashMap<pid_t, (Worker, Sender<Task>)>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    total_workers: usize,
}

impl WorkerPool {
    fn new(size: usize) -> Self {
        info!("Creating WorkerPool with capacity for {} workers", size);
        Self {
            available_workers: Arc::new(Mutex::new(HashMap::with_capacity(size))),
            worker_handles: Arc::new(Mutex::new(Vec::with_capacity(size))),
            total_workers: size,
        }
    }

    fn add_worker(&self, worker: Worker, handle: JoinHandle<()>, tx: Sender<Task>) {
        let mut workers = self.available_workers.lock().unwrap();
        let mut handles = self.worker_handles.lock().unwrap();

        workers.insert(worker.tid, (worker, tx));
        handles.push(handle);
    }

    fn get_worker(&self) -> Option<(Worker, Sender<Task>)> {
        let mut workers = self.available_workers.lock().unwrap();
        // Get and remove the first available worker
        if let Some((&worker_tid, _)) = workers.iter().next() {
            workers.remove(&worker_tid)
        } else {
            None
        }
    }

    fn return_worker(&self, worker: Worker, tx: Sender<Task>) {
        debug!("WorkerPool: Returning worker {} to pool", worker.id);
        self.available_workers.lock().unwrap().insert(worker.tid, (worker, tx));
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

            // Rest of worker thread logic remains the same
            debug!("Worker {}: Entering task processing loop", self.id);
            while let Ok(task) = self.task_rx.recv() {
                debug!("Worker {}: Received task from channel", self.id);
                match task {
                    Task::Function(task) => {
                        info!("Worker {}: Starting task execution", self.id);
                        task(&self.handle);
                        info!("Worker {}: Completed task execution", self.id);

                        debug!("Worker {} [{}]: Signaling ready for more work", self.id, self.tid);
                        let wait_result = sys_umcg_ctl(
                            self.server_id as u64,  // Include server ID in wait call
                            UmcgCmd::Wait,
                            0,
                            0,
                            None,
                            0
                        );
                        debug!("Worker {} [{}]: Wait syscall returned {}",
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
    worker_states: Arc<Mutex<HashMap<pid_t, WorkerState>>>,
    completed_cycles: Arc<Mutex<HashMap<Uuid, bool>>>,
    done: Arc<AtomicBool>,
    cpu_id: usize,
}

impl Server {
    fn new(id: usize, cpu_id: usize, executor: Arc<Executor>) -> Self {
        log_with_timestamp(&format!("Creating Server {}", id));
        // Get worker_count from executor's config
        let worker_count = executor.config.worker_count;
        Self {
            id,
            task_queue: Arc::new(Mutex::new(TaskQueue::new())),
            worker_pool: Arc::new(WorkerPool::new(worker_count)), // Use worker_count instead of config
            executor,
            worker_states: Arc::new(Mutex::new(HashMap::new())),
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            done: Arc::new(AtomicBool::new(false)),
            cpu_id
        }
    }
    fn initialize_workers(&self) -> Result<(), ServerError> {
        info!("Server {}: Initializing workers", self.id);

        for worker_id in 0..self.worker_pool.total_workers {
            info!("Server {}: Initializing worker {}", self.id, worker_id);

            // Create new worker thread
            let (tx, rx) = channel();
            let worker_thread = WorkerThread::new(
                worker_id,
                self.id,
                self.cpu_id,
                self.executor.clone(),
                rx,
            );

            // Update state to initializing
            self.transition_worker_state(0, WorkerStatus::Initializing, None);

            // Start the worker thread
            let (handle, tid) = worker_thread.start();

            // Update state to registering
            self.transition_worker_state(tid, WorkerStatus::Registering, None);


            // Wait for initial wake event with timeout
            let worker_event = self.wait_for_worker_registration(worker_id)?;
            debug!("Worker {}: Registering worker event {:?}", self.id, worker_event);

            // Create worker instance and add to pool
            let worker = Worker {
                id: worker_id,
                tid,
                handle: TaskHandle { executor: self.executor.clone() },
            };

            self.worker_pool.add_worker(worker, handle, tx);
            self.transition_worker_state(tid, WorkerStatus::Waiting, None);

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

    pub fn add_task(&self, task: Task) {  // Make this public
        log_with_timestamp(&format!("Server {}: Adding new task", self.id));
        match task {
            Task::Function(f) => {
                let task_entry = TaskEntry::new(f);
                let mut queue = self.task_queue.lock().unwrap();
                queue.enqueue(task_entry);
                log_with_timestamp(&format!("Server {}: Task queued, attempting to process", self.id));
                drop(queue);
                self.process_next_task();
            }
            Task::Shutdown => {
                log_with_timestamp(&format!("Server {}: Received shutdown signal", self.id));
                self.done.store(true, Ordering::Relaxed);
            }
        }
    }

    fn transition_worker_state(&self, worker_tid: pid_t, new_state: WorkerStatus, task_id: Option<Uuid>) {
        let mut states = self.worker_states.lock().unwrap();
        let old_state = states.get(&worker_tid).map(|s| s.status.clone());
        states.entry(worker_tid)
            .and_modify(|state| {
                debug!("Server {}: Transitioning worker {} from {:?} to {:?}",
                    self.id, worker_tid, old_state, new_state.clone());
                state.status = new_state.clone();
                state.current_task = task_id;
            })
            .or_insert_with(|| {
                debug!("Server {}: Inserting worker state {:?}", self.id, new_state.clone());
                let mut state = WorkerState::new(worker_tid, self.id);
                state.status = new_state.clone();
                state.current_task = task_id;
                state
            });
        drop(states);
    }

    fn process_next_task(&self) -> bool {
        debug!("Server {}: Attempting to process next task", self.id);
        let mut task_queue = self.task_queue.lock().unwrap();

        if let Some(task) = task_queue.get_next_task() {
            debug!("Server {}: Got task, checking worker pool", self.id);
            if let Some((worker, tx)) = self.worker_pool.get_worker() {
                debug!("Server {}: Got worker {} from pool", self.id, worker.id);
                info!("Server {}: Assigning task to worker {}", self.id, worker.id);

                // Update state before assigning task
                self.transition_worker_state(worker.tid, WorkerStatus::Running, Some(task.id));
                task_queue.mark_in_progress(task.id, worker.tid);

                let task = match worker.assign_task(task, &tx) {
                    Ok(()) => {
                        debug!("Server {}: Context switching to worker {}", self.id, worker.tid);
                        let mut events = [0u64; 2];
                        let switch_result = sys_umcg_ctl(
                            0,
                            UmcgCmd::CtxSwitch,
                            worker.tid,
                            0,
                            Some(&mut events),
                            2
                        );
                        debug!("Server {}: Context switch result: {}", self.id, switch_result);

                        if switch_result != 0 {
                            // Context switch failed - revert worker state and return to pool
                            debug!("Server {}: Context switch failed for worker {}, reverting state",
                            self.id, worker.tid);
                            self.transition_worker_state(worker.tid, WorkerStatus::Waiting, None);
                            self.worker_pool.return_worker(worker, tx);
                            return false;
                        } else {
                            debug!("Server {}: Successfully switched to worker {}", self.id, worker.tid);
                            return true;
                        }
                    }
                    Err(task) => task
                };

                // If we get here, assignment failed
                task_queue.enqueue(task);
                false
            } else {
                debug!("Server {}: No workers available in pool", self.id);
                task_queue.enqueue(task);
                false
            }
        } else {
            debug!("Server {}: No tasks in queue", self.id);
            false
        }
    }

    fn handle_umcg_event(&self, event: u64) -> Result<(), ServerError> {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        debug!("Server {}: Processing event {} for worker {}",
            self.id, event_type, worker_tid);

        match event_type {
            1 => { // BLOCK
                debug!("Server {}: Worker {} blocked", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Blocked, None);
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
                 *
                 * This state-based approach allows us to handle the UMCG implementation's
                 * behavior where Wait operations manifest as Wake events to the server.
                 */
                let states = self.worker_states.lock().unwrap();
                let current_status = states.get(&worker_tid).map(|s| s.status.clone());
                let current_task = states.get(&worker_tid).and_then(|s| s.current_task);
                drop(states);

                debug!("Server {}: Worker {} woke up with current status: {:?}",
                    self.id, worker_tid, current_status);

                match current_status {
                    Some(WorkerStatus::Running) => {
                        debug!("Server {}: Worker {} completing task", self.id, worker_tid);
                        self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);

                        if let Some(task_id) = current_task {
                            let mut task_queue = self.task_queue.lock().unwrap();
                            task_queue.in_progress.remove(&task_id);
                        }

                        // Return worker to pool
                        if let Some((worker, tx)) = self.worker_pool.available_workers.lock().unwrap()
                            .values()
                            .find(|(w, _)| w.tid == worker_tid)
                            .cloned()
                        {
                            self.worker_pool.return_worker(worker, tx);
                        }
                    },
                    Some(WorkerStatus::Blocked) => {
                        debug!("Server {}: Worker {} unblocking", self.id, worker_tid);
                        self.transition_worker_state(worker_tid, WorkerStatus::Running, current_task);
                    },
                    _ => {
                        debug!("Server {}: Worker {} in default state, setting to Waiting",
                            self.id, worker_tid);
                        self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);
                    }
                }
            },
            3 => { // WAIT
                debug!("Server {}: Worker {} waiting", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);
            },
            4 => { // EXIT
                debug!("Server {}: Worker {} exited", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Completed, None);
            },
            _ => {
                debug!("Server {}: Unknown event {} from worker {}",
                    self.id, event_type, worker_tid);
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
        while !self.done.load(Ordering::Relaxed) {
            let mut events = [0u64; 6];
            let ret = umcg_wait_retry(0, Some(&mut events), 6);

            if ret != 0 {
                error!("Server {} wait error: {}", self.id, ret);
                return Err(ServerError::SystemError(std::io::Error::last_os_error()));
            }

            for &event in events.iter().take_while(|&&e| e != 0) {
                // Use our existing event handler
                if let Err(e) = self.handle_umcg_event(event) {
                    error!("Server {} event handling error: {}", self.id, e);
                }

                // After each event, check if we can schedule tasks
                // This lets us respond immediately when workers become available
                self.try_schedule_tasks()?;
            }
        }

        self.initiate_shutdown();
        info!("Server {}: Shutdown complete", self.id);
        Ok(())
    }

    fn try_schedule_tasks(&self) -> Result<(), ServerError> {
        // Only attempt to schedule if we have available workers
        if let Some((worker, tx)) = self.worker_pool.get_worker() {
            let mut task_queue = self.task_queue.lock().unwrap();
            if let Some(task) = task_queue.get_next_task() {
                info!("Server {}: Assigning task to worker {}", self.id, worker.id);

                self.transition_worker_state(worker.tid, WorkerStatus::Running, Some(task.id));
                task_queue.mark_in_progress(task.id, worker.tid);
                drop(task_queue);

                match worker.assign_task(task, &tx) {
                    Ok(()) => {
                        match self.context_switch_worker(worker.tid) {
                            Ok(()) => {
                                debug!("Server {}: Successfully switched to worker {}",
                                self.id, worker.tid);
                            }
                            Err(e) => {
                                debug!("Server {}: Context switch failed for worker {}: {}",
                                self.id, worker.tid, e);
                                self.transition_worker_state(worker.tid, WorkerStatus::Waiting, None);
                                self.worker_pool.return_worker(worker, tx);
                                return Err(e);
                            }
                        }
                    }
                    Err(failed_task) => {
                        // Assignment failed, requeue task
                        let mut task_queue = self.task_queue.lock().unwrap();
                        task_queue.enqueue(failed_task);
                        self.worker_pool.return_worker(worker, tx);
                    }
                }
            } else {
                // No tasks, return worker to pool
                self.worker_pool.return_worker(worker, tx);
            }
        }
        Ok(())
    }

    fn initiate_shutdown(&self) {
        info!("Server {}: Beginning shutdown sequence", self.id);
        self.done.store(true, Ordering::SeqCst);

        // Signal all workers to shutdown
        {
            let workers = self.worker_pool.available_workers.lock().unwrap();
            for (worker_tid, (worker, tx)) in workers.iter() {
                debug!("Server {}: Sending shutdown signal to worker {}", self.id, worker.id);
                if let Err(e) = tx.send(Task::Shutdown) {
                    error!("Server {}: Failed to send shutdown to worker {}: {}", self.id, worker.id, e);
                }
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

        // Wrap the task with completion tracking
        let stats = self.task_stats.clone();
        let wrapped_task = Box::new(move |handle: &TaskHandle| {
            f(handle);
            stats.mark_completed(task_id);
        });

        let server_idx = self.next_server.fetch_add(1, Ordering::Relaxed) % self.config.server_count;
        let servers = self.servers.lock().unwrap();

        if let Some(server) = servers.get(server_idx) {
            let worker_count = server.worker_pool.available_workers.lock().unwrap().len();
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
        debug!("UMCG wait retry - worker: {}, flags: {}", worker_id, flags);
        let events = events_buf.as_deref_mut();
        let ret = sys_umcg_ctl(
            flags,
            UmcgCmd::Wait,
            (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            events,
            event_sz,
        );
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
            log_with_timestamp(&format!("Initial task {}: Starting task", i));
            thread::sleep(Duration::from_secs(2));
            log_with_timestamp(&format!("Initial task {}: Preparing to spawn child task", i));

            let parent_id = i;
            handle.submit(move |_| {
                log_with_timestamp(&format!("Child task of initial task {}: Starting work", parent_id));
                thread::sleep(Duration::from_secs(1));
                log_with_timestamp(&format!("Child task of initial task {}: Completed", parent_id));
            });

            log_with_timestamp(&format!("Initial task {}: Completed", i));
        };

        log_with_timestamp(&format!("Submitting initial task {}", i));
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

pub fn test_worker_first() -> i32 {
    println!("Testing worker registration before server...");

    // Start worker thread
    let worker_handle = thread::spawn(|| {
        let tid = get_thread_id();
        println!("Worker: registering with tid {}", tid);

        // Register worker
        assert_eq!(
            sys_umcg_ctl(
                0,
                UmcgCmd::RegisterWorker,
                0,
                (tid as u64) << UMCG_WORKER_ID_SHIFT,
                None,
                0
            ),
            0
        );
        println!("Worker: registered, starting sleep");
        thread::sleep(Duration::from_secs(2));
        println!("Worker: sleep complete");
    });

    // Give worker time to register
    thread::sleep(Duration::from_millis(100));

    // Start server
    println!("Starting server...");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    // Server event loop
    for _ in 0..10 {
        println!("Server: waiting for events...");
        let mut events = [0u64; 6];
        let ret = umcg_wait_retry(0, Some(&mut events), 6);
        assert_eq!(ret, 0);

        // Process each event
        for &event in events.iter().take_while(|&&e| e != 0) {
            let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
            let worker_tid = event >> UMCG_WORKER_ID_SHIFT;
            println!("Server: got event type {} from worker {}", event_type, worker_tid);

            // If we got a WAKE event, try context switching to the worker
            if event_type == 2 { // WAKE
                println!("Server: context switching to worker {}", worker_tid);
                let mut switch_events = [0u64; 6];
                let ret = sys_umcg_ctl(
                    0,
                    UmcgCmd::CtxSwitch,
                    worker_tid as i32,
                    0,
                    Some(&mut switch_events),
                    6
                );
                assert_eq!(ret, 0);
                println!("Server: context switch returned");

                // Process any events from the context switch
                for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
                    let switch_event_type = switch_event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
                    let switch_worker_tid = switch_event >> UMCG_WORKER_ID_SHIFT;
                    println!("Server: after switch got event type {} from worker {}",
                             switch_event_type, switch_worker_tid);
                }
            }
        }
    }

    worker_handle.join().unwrap();
    0
}

pub fn test_server_first() -> i32 {
    println!("Testing server registration before worker...");

    // Start server first
    println!("Starting server...");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    // Start worker thread
    let worker_handle = thread::spawn(|| {
        let tid = get_thread_id();
        println!("Worker: registering with tid {}", tid);

        // Register worker
        assert_eq!(
            sys_umcg_ctl(
                0,
                UmcgCmd::RegisterWorker,
                0,
                (tid as u64) << UMCG_WORKER_ID_SHIFT,
                None,
                0
            ),
            0
        );
        println!("Worker: registered, starting sleep");
        thread::sleep(Duration::from_secs(2));
        println!("Worker: sleep complete");
    });

    // Server event loop
    for _ in 0..10 {
        println!("Server: waiting for events...");
        let mut events = [0u64; 6];
        let ret = umcg_wait_retry(0, Some(&mut events), 6);
        assert_eq!(ret, 0);

        // Process each event
        for &event in events.iter().take_while(|&&e| e != 0) {
            let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
            let worker_tid = event >> UMCG_WORKER_ID_SHIFT;
            println!("Server: got event type {} from worker {}", event_type, worker_tid);

            // If we got a WAKE event, try context switching to the worker
            if event_type == 2 { // WAKE
                println!("Server: context switching to worker {}", worker_tid);
                let mut switch_events = [0u64; 6];
                let ret = sys_umcg_ctl(
                    0,
                    UmcgCmd::CtxSwitch,
                    worker_tid as i32,
                    0,
                    Some(&mut switch_events),
                    6
                );
                assert_eq!(ret, 0);
                println!("Server: context switch returned");

                // Process any events from the context switch
                for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
                    let switch_event_type = switch_event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
                    let switch_worker_tid = switch_event >> UMCG_WORKER_ID_SHIFT;
                    println!("Server: after switch got event type {} from worker {}",
                             switch_event_type, switch_worker_tid);
                }
            }
        }
    }

    worker_handle.join().unwrap();
    0
}