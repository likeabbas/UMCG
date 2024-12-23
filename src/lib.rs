use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};
use std::collections::{VecDeque, HashMap, HashSet};
use uuid::Uuid;

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
                // Test channel first before moving task_fn
                match tx.send(Task::Shutdown) {
                    Ok(_) => {
                        // Channel works, now we can safely extract and send task_fn
                        if let TaskState::Pending(task_fn) = std::mem::replace(&mut task.state, TaskState::Completed) {
                            // Send the actual task
                            let _ = tx.send(Task::Function(task_fn));
                            debug!("Worker {}: Task sent successfully", self.id);
                            Ok(())
                        } else {
                            unreachable!()
                        }
                    },
                    Err(_) => {
                        debug!("Worker {}: Channel is disconnected", self.id);
                        Err(task)  // Return original task untouched
                    }
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

struct WorkerPool {
    available_workers: Arc<Mutex<VecDeque<(Worker, Sender<Task>)>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    worker_channels: Arc<Mutex<HashMap<pid_t, Sender<Task>>>>,  // Store channels permanently
    total_workers: usize,
}

impl WorkerPool {
    fn new(size: usize, server_id: usize, cpu_id: usize, executor: Arc<Executor>) -> Self {
        log_with_timestamp(&format!("Creating WorkerPool with {} workers for CPU {} on server {}",
                                    size, cpu_id, server_id));
        let mut workers = VecDeque::with_capacity(size);
        let mut handles = Vec::with_capacity(size);
        let mut channels = HashMap::new();

        for id in 0..size {
            log_with_timestamp(&format!("Initializing worker {} for CPU {} on server {}",
                                        id, cpu_id, server_id));
            let (tx, rx) = channel();

            let worker_thread = WorkerThread {
                id,
                tid: 0,
                task_rx: rx,
                handle: TaskHandle { executor: executor.clone() },
                server_id,
                cpu_id,
            };

            let (handle, tid) = worker_thread.start();
            handles.push(handle);

            let worker = Worker {
                id,
                tid,
                handle: TaskHandle { executor: executor.clone() },
            };

            // Store channel permanently
            channels.insert(tid, tx.clone());

            log_with_timestamp(&format!("Adding worker {} to pool", id));
            workers.push_back((worker, tx));
        }

        Self {
            available_workers: Arc::new(Mutex::new(workers)),
            worker_handles: Arc::new(Mutex::new(handles)),
            worker_channels: Arc::new(Mutex::new(channels)),
            total_workers: size,
        }
    }

    fn get_worker(&self) -> Option<(Worker, Sender<Task>)> {
        let mut workers = self.available_workers.lock().unwrap();
        let result = workers.pop_front();
        if let Some((ref worker, _)) = result {
            log_with_timestamp(&format!("WorkerPool: Retrieved worker {}", worker.id));
        } else {
            log_with_timestamp("WorkerPool: No workers available");
        }
        result
    }

    fn return_worker(&self, worker: Worker, tx: Sender<Task>) {
        debug!("WorkerPool: Returning worker {} to pool", worker.id);
        self.available_workers.lock().unwrap().push_back((worker, tx));
    }

    fn get_task_sender(&self, tid: pid_t) -> Option<Sender<Task>> {
        self.worker_channels.lock().unwrap().get(&tid).cloned()
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

            // Rest of worker thread logic remains the same
            while let Ok(task) = self.task_rx.recv() {
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
    fn new(id: usize, cpu_id: usize, executor: Arc<Executor>, worker_pool: Arc<WorkerPool>) -> Self {
        log_with_timestamp(&format!("Creating Server {}", id));
        let mut worker_states = HashMap::new();

        // Create a new scope for the workers lock
        {
            let workers = worker_pool.available_workers.lock().unwrap();
            for (worker, _) in workers.iter() {
                worker_states.insert(worker.tid, WorkerState::new(worker.tid, id));
            }
        } // Lock is dropped here

        Self {
            id,
            task_queue: Arc::new(Mutex::new(TaskQueue::new())),
            worker_pool, // Now this move is valid
            executor,
            worker_states: Arc::new(Mutex::new(worker_states)),
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            done: Arc::new(AtomicBool::new(false)),
            cpu_id
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
            if let Some((worker, tx)) = self.worker_pool.get_worker() {
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
                task_queue.enqueue(task);
                false
            }
        } else {
            false
        }
    }

    fn process_event(&self, event: u64) {
        log_with_timestamp(&format!("Server {}: Raw event value: 0x{:x}", self.id, event));
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        debug!("Server {}: Processing event {} for worker {}", self.id, event_type, worker_tid);

        match event_type {
            1 => { // BLOCK
                debug!("Server {}: Worker {} blocked", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Blocked, None);
                self.process_next_task();
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

                        // Update state first
                        self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);

                        // Clean up completed task if any
                        if let Some(task_id) = current_task {
                            let mut task_queue = self.task_queue.lock().unwrap();
                            task_queue.in_progress.remove(&task_id);
                            drop(task_queue);
                            debug!("Server {}: Cleaned up completed task {}", self.id, task_id);
                        }

                        // Return worker to pool
                        if let Some((worker, tx)) = self.worker_pool.available_workers.lock().unwrap()
                            .iter()
                            .find(|(w, _)| w.tid == worker_tid)
                            .cloned()
                        {
                            debug!("Server {}: Returning worker {} to pool", self.id, worker_tid);
                            self.worker_pool.return_worker(worker, tx);
                            self.process_next_task();
                        }
                    },
                    Some(WorkerStatus::Blocked) => {
                        debug!("Server {}: Worker {} unblocking", self.id, worker_tid);
                        self.transition_worker_state(worker_tid, WorkerStatus::Running, current_task);

                        let mut events = [0u64; 2];
                        let switch_result = sys_umcg_ctl(
                            0,
                            UmcgCmd::CtxSwitch,
                            worker_tid,
                            0,
                            Some(&mut events),
                            2
                        );
                        debug!("Server {}: Resume context switch result: {}", self.id, switch_result);

                        if switch_result != 0 {
                            debug!("Server {}: Failed to resume blocked worker {}", self.id, worker_tid);
                            // If context switch fails, transition back to blocked
                            self.transition_worker_state(worker_tid, WorkerStatus::Blocked, current_task);
                        }
                    },
                    Some(WorkerStatus::Waiting) => {
                        debug!("Server {}: Worker {} already waiting", self.id, worker_tid);
                        self.process_next_task();
                    },
                    _ => {
                        debug!("Server {}: Worker {} in unexpected state, setting to Waiting", self.id, worker_tid);
                        self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);
                    }
                }
            },
            3 => { // WAIT
                debug!("Server {}: Worker {} waiting", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Waiting, None);
                self.process_next_task();
            },
            4 => { // EXIT
                debug!("Server {}: Worker {} exited", self.id, worker_tid);
                self.transition_worker_state(worker_tid, WorkerStatus::Completed, None);
            },
            _ => debug!("Server {}: Unknown event {} from worker {}",
                                             self.id, event_type, worker_tid),
        }
    }

    fn start(self) -> JoinHandle<()> {
        thread::spawn(move || {
            if let Err(e) = set_cpu_affinity(self.cpu_id) {
                log_with_timestamp(&format!("Could not set CPU affinity for server {} to CPU {}: {}",
                                            self.id, self.cpu_id, e));
            }

            log_with_timestamp(&format!("Server {}: Starting up", self.id));

            let reg_result = sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0);
            assert_eq!(reg_result, 0, "Server {} UMCG registration failed", self.id);
            log_with_timestamp(&format!("Server {}: UMCG registration complete", self.id));

            while !self.done.load(Ordering::Relaxed) {
                let mut events = [0u64; 6];
                log_with_timestamp(&format!("Server {}: Waiting for events", self.id));
                let ret = umcg_wait_retry(0, Some(&mut events), 6);

                if ret != 0 {
                    log_with_timestamp(&format!("Server {} wait error: {}", self.id, ret));
                    break;
                }

                for &event in events.iter().take_while(|&&e| e != 0) {
                    log_with_timestamp(&format!("Server {}: Processing event: {}", self.id, event));
                    self.process_event(event);
                }
            }

            log_with_timestamp(&format!("Server {}: Beginning shutdown", self.id));
            let unreg_result = sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
            assert_eq!(unreg_result, 0, "Server {} UMCG unregistration failed", self.id);
            log_with_timestamp(&format!("Server {}: Shutdown complete", self.id));
        })
    }
}

struct Executor {
    servers: Mutex<Vec<Server>>,
    next_server: AtomicUsize,
    config: ExecutorConfig,
}

impl Executor {
    fn new(config: ExecutorConfig) -> Arc<Self> {
        log_with_timestamp("Creating new Executor");
        let executor = Arc::new(Self {
            servers: Mutex::new(Vec::with_capacity(config.server_count)),
            next_server: AtomicUsize::new(0),
            config: config.clone(),
        });

        let executor_clone = executor.clone();
        {
            let mut servers = executor_clone.servers.lock().unwrap();

            for i in 0..config.server_count {
                let cpu_id = config.start_cpu + i;
                log_with_timestamp(&format!("Creating Server {} on CPU {}", i, cpu_id));

                // Create worker pool with CPU affinity
                let worker_pool = Arc::new(WorkerPool::new(
                    config.worker_count,
                    i,  // server_id
                    cpu_id,  // cpu_id
                    executor_clone.clone()
                ));

                // Create server with CPU affinity
                servers.push(Server::new(i, cpu_id, executor_clone.clone(), worker_pool));
            }
        }

        executor
    }

    fn submit(&self, task: Box<dyn FnOnce(&TaskHandle) + Send>) {
        info!("Executor: Submitting new task");
        let server_idx = self.next_server.fetch_add(1, Ordering::Relaxed) % self.config.server_count;

        let servers = self.servers.lock().unwrap();
        if let Some(server) = servers.get(server_idx) {
            let worker_count = server.worker_pool.available_workers.lock().unwrap().len();
            info!("Executor: Adding task to server {} (has {} workers)", server_idx, worker_count);
            server.add_task(Task::Function(task));
            debug!("Task assigned to server {}", server_idx);
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

    thread::sleep(Duration::from_millis(100));

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
    thread::sleep(Duration::from_secs(10));
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