use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};
use std::collections::{VecDeque, HashMap};
use uuid::Uuid;

const SYS_UMCG_CTL: i64 = 450;
const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;

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

#[derive(Debug, Clone, PartialEq)]
enum WorkerStatus {
    Running,
    Blocked,
    Waiting,  // Ready for new tasks
    Completed,
}

struct WorkerState {
    tid: pid_t,
    status: WorkerStatus,
    current_task: Option<Uuid>,
}

impl WorkerState {
    fn new(tid: pid_t) -> Self {
        log_with_timestamp(&format!("Creating new WorkerState for tid {}", tid));
        Self {
            tid,
            status: WorkerStatus::Running,
            current_task: None,
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
        log_with_timestamp(&format!("Creating new TaskEntry with ID: {}", id));
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
        log_with_timestamp("Creating new TaskQueue");
        Self {
            pending: VecDeque::new(),
            preempted: VecDeque::new(),
            in_progress: HashMap::new(),
            mutex: Mutex::new(()),
        }
    }

    fn enqueue(&mut self, task: TaskEntry) {
        let _guard = self.mutex.lock().unwrap();
        log_with_timestamp(&format!("TaskQueue: Enqueueing task {}", task.id));
        match &task.state {
            TaskState::Pending(_) => {
                log_with_timestamp(&format!("TaskQueue: Task {} added to pending queue", task.id));
                self.pending.push_back(task)
            },
            TaskState::Running { preempted: true, .. } => {
                log_with_timestamp(&format!("TaskQueue: Task {} added to preempted queue", task.id));
                self.preempted.push_back(task)
            },
            TaskState::Running { .. } => {
                log_with_timestamp(&format!("TaskQueue: Task {} added to in_progress map", task.id));
                self.in_progress.insert(task.id, task);
            }
            TaskState::Completed => {
                log_with_timestamp(&format!("Warning: Attempting to enqueue completed task {}", task.id));
            }
        }
        log_with_timestamp(&format!("TaskQueue stats - Pending: {}, Preempted: {}, In Progress: {}",
                                    self.pending.len(), self.preempted.len(), self.in_progress.len()));
    }

    fn get_next_task(&mut self) -> Option<TaskEntry> {
        let _guard = self.mutex.lock().unwrap();
        log_with_timestamp("TaskQueue: Attempting to get next task");
        let task = self.preempted
            .pop_front()
            .or_else(|| self.pending.pop_front());

        if let Some(ref task) = task {
            log_with_timestamp(&format!("TaskQueue: Retrieved task {}", task.id));
        } else {
            log_with_timestamp("TaskQueue: No tasks available");
        }

        task
    }

    fn mark_in_progress(&mut self, task_id: Uuid, worker_tid: i32) {
        let _guard = self.mutex.lock().unwrap();
        log_with_timestamp(&format!("TaskQueue: Marking task {} as in progress with worker {}", task_id, worker_tid));
        if let Some(task) = self.in_progress.get_mut(&task_id) {
            if let TaskState::Pending(_) = task.state {
                task.state = TaskState::Running {
                    worker_tid,
                    start_time: SystemTime::now(),
                    preempted: false,
                    blocked: false,
                    state: WorkerStatus::Running,
                };
                log_with_timestamp(&format!("TaskQueue: Task {} state updated to Running", task_id));
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
        self.executor.submit(Box::new(f));
    }
}

#[derive(Clone)]
struct ExecutorConfig {
    server_count: usize,
    worker_count: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            server_count: 1,
            worker_count: 3,
        }
    }
}

#[derive(Clone)]
struct Worker {
    id: usize,
    tid: pid_t,
    handle: TaskHandle,
}

struct WorkerThread {
    id: usize,
    tid: pid_t,
    task_rx: Receiver<Task>,
    handle: TaskHandle,
}

impl Worker {
    fn assign_task(&self, mut task: TaskEntry, tx: &Sender<Task>) {
        log_with_timestamp(&format!("Worker {}: Starting task assignment", self.id));
        if let TaskState::Pending(task_fn) = std::mem::replace(&mut task.state, TaskState::Completed) {
            log_with_timestamp(&format!("Worker {}: Sending task to execution channel", self.id));
            match tx.send(Task::Function(task_fn)) {
                Ok(_) => log_with_timestamp(&format!("Worker {}: Task sent successfully", self.id)),
                Err(e) => log_with_timestamp(&format!("Worker {}: Failed to send task: {}", self.id, e)),
            }
        }
        log_with_timestamp(&format!("Worker {}: Task assignment completed", self.id));
    }
}

impl WorkerThread {
    fn start(mut self) -> (JoinHandle<()>, pid_t) {
        let (tid_tx, tid_rx) = channel();
        let id = self.id;
        log_with_timestamp(&format!("Starting WorkerThread {}", id));

        let handle = thread::spawn(move || {
            self.tid = get_thread_id();
            log_with_timestamp(&format!("Worker {}: Initialized with tid {}", self.id, self.tid));

            tid_tx.send(self.tid).expect("Failed to send worker tid");

            let worker_id = (self.tid as u64) << UMCG_WORKER_ID_SHIFT;
            log_with_timestamp(&format!("Worker {}: Registering with UMCG (worker_id: {})", self.id, worker_id));

            let reg_result = sys_umcg_ctl(
                0,
                UmcgCmd::RegisterWorker,
                0,
                worker_id,
                None,
                0
            );
            assert_eq!(reg_result, 0, "Worker {} UMCG registration failed", self.id);
            log_with_timestamp(&format!("Worker {}: UMCG registration complete", self.id));

            while let Ok(task) = self.task_rx.recv() {
                match task {
                    Task::Function(task) => {
                        log_with_timestamp(&format!("Worker {}: Starting task execution", self.id));
                        task(&self.handle);
                        log_with_timestamp(&format!("Worker {}: Completed task execution", self.id));

                        log_with_timestamp(&format!("Worker {} [{}]: Signaling ready for more work", self.id, self.tid));
                        let wait_result = sys_umcg_ctl(
                            0,
                            UmcgCmd::Wait,
                            0,
                            0,
                            None,
                            0
                        );
                        log_with_timestamp(&format!("Worker {} [{}]: Wait syscall returned {}",
                                                    self.id, self.tid, wait_result));
                        assert_eq!(wait_result, 0, "Worker {} UMCG wait failed", self.id);
                    }
                    Task::Shutdown => {
                        log_with_timestamp(&format!("Worker {}: Shutting down", self.id));
                        break;
                    }
                }
            }

            log_with_timestamp(&format!("Worker {}: Beginning shutdown", self.id));
            let unreg_result = sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
            assert_eq!(unreg_result, 0, "Worker {} UMCG unregistration failed", self.id);
            log_with_timestamp(&format!("Worker {}: Shutdown complete", self.id));
        });

        let tid = tid_rx.recv().expect("Failed to receive worker tid");
        log_with_timestamp(&format!("WorkerThread {} started with tid {}", id, tid));
        (handle, tid)
    }
}

struct WorkerPool {
    available_workers: Arc<Mutex<VecDeque<(Worker, Sender<Task>)>>>,
    worker_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    total_workers: usize,
}

impl WorkerPool {
    fn new(size: usize, executor: Arc<Executor>) -> Self {
        log_with_timestamp(&format!("Creating WorkerPool with {} workers", size));
        let mut workers = VecDeque::with_capacity(size);
        let mut handles = Vec::with_capacity(size);

        for id in 0..size {
            log_with_timestamp(&format!("Initializing worker {}", id));
            let (tx, rx) = channel();

            let worker_thread = WorkerThread {
                id,
                tid: 0,
                task_rx: rx,
                handle: TaskHandle { executor: executor.clone() },
            };

            let (handle, tid) = worker_thread.start();
            handles.push(handle);

            let worker = Worker {
                id,
                tid,
                handle: TaskHandle { executor: executor.clone() },
            };

            log_with_timestamp(&format!("Adding worker {} to available pool", id));
            workers.push_back((worker, tx));
        }

        Self {
            available_workers: Arc::new(Mutex::new(workers)),
            worker_handles: Arc::new(Mutex::new(handles)),
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
        log_with_timestamp(&format!("WorkerPool: Returning worker {} to pool", worker.id));
        self.available_workers.lock().unwrap().push_back((worker, tx));
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
        let server_count = config.server_count;
        let executor = Arc::new(Self {
            servers: Mutex::new(Vec::with_capacity(server_count)),
            next_server: AtomicUsize::new(0),
            config: config.clone(),
        });

        let executor_clone = executor.clone();
        {
            let mut servers = executor_clone.servers.lock().unwrap();
            log_with_timestamp("Creating WorkerPool");
            let worker_pool = Arc::new(WorkerPool::new(
                config.worker_count,
                executor_clone.clone()
            ));

            for i in 0..server_count {
                log_with_timestamp(&format!("Creating Server {}", i));
                servers.push(Server::new(i, executor_clone.clone(), worker_pool.clone()));
            }
        }

        executor
    }

    fn submit(&self, task: Box<dyn FnOnce(&TaskHandle) + Send>) {
        log_with_timestamp("Executor: Submitting new task");

        let server_idx = self.next_server.fetch_add(1, Ordering::Relaxed) % self.config.server_count;

        let servers = self.servers.lock().unwrap();
        if let Some(server) = servers.get(server_idx) {
            server.add_task(Task::Function(task));
            log_with_timestamp(&format!("Task assigned to server {}", server_idx));
        }
    }

    fn start(&self) {
        log_with_timestamp("Executor: Starting servers");
        let servers = self.servers.lock().unwrap();
        for server in servers.iter() {
            server.clone().start();
        }
        log_with_timestamp("Executor: All servers started");
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
}

impl Server {
    fn new(id: usize, executor: Arc<Executor>, worker_pool: Arc<WorkerPool>) -> Self {
        log_with_timestamp(&format!("Creating Server {}", id));
        Self {
            id,
            task_queue: Arc::new(Mutex::new(TaskQueue::new())),
            worker_pool,
            executor,
            worker_states: Arc::new(Mutex::new(HashMap::new())),
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn add_task(&self, task: Task) {
        log_with_timestamp(&format!("Server {}: Adding new task", self.id));
        match task {
            Task::Function(f) => {
                let task_entry = TaskEntry::new(f);
                let mut queue = self.task_queue.lock().unwrap();
                queue.enqueue(task_entry);
                log_with_timestamp(&format!("Server {}: Task queued, attempting to process", self.id));
                drop(queue);  // Release the lock before processing
                self.process_next_task();
            }
            Task::Shutdown => {
                log_with_timestamp(&format!("Server {}: Received shutdown signal", self.id));
                self.done.store(true, Ordering::Relaxed);
            }
        }
    }

    fn can_assign_to_worker(&self, worker_tid: pid_t) -> bool {
        let states = self.worker_states.lock().unwrap();
        if let Some(state) = states.get(&worker_tid) {
            let can_assign = matches!(state.status, WorkerStatus::Waiting);
            log_with_timestamp(&format!("Server {}: Worker {} status check - current: {:?}, can_assign: {}",
                                        self.id, worker_tid, state.status, can_assign));
            can_assign
        } else {
            log_with_timestamp(&format!("Server {}: Worker {} has no state record", self.id, worker_tid));
            false
        }
    }

    fn process_next_task(&self) -> bool {
        log_with_timestamp(&format!("Server {}: Attempting to process next task", self.id));
        let mut task_queue = self.task_queue.lock().unwrap();

        if let Some(task) = task_queue.get_next_task() {
            match &task.state {
                TaskState::Pending(_) => {
                    log_with_timestamp(&format!("Server {}: Found pending task", self.id));
                    if let Some((worker, tx)) = self.worker_pool.get_worker() {
                        if self.can_assign_to_worker(worker.tid) {
                            log_with_timestamp(&format!("Server {}: Got worker {} for task", self.id, worker.id));
                            task_queue.mark_in_progress(task.id, worker.tid);

                            // Update worker state
                            let mut states = self.worker_states.lock().unwrap();
                            if let Some(state) = states.get_mut(&worker.tid) {
                                log_with_timestamp(&format!("Server {}: Worker {} state transition {:?} -> Running",
                                                            self.id, worker.tid, state.status));
                                state.status = WorkerStatus::Running;
                                state.current_task = Some(task.id);
                            }
                            drop(states);

                            worker.assign_task(task, &tx);

                            log_with_timestamp(&format!("Server {}: Context switching to worker {} (tid {})",
                                                        self.id, worker.id, worker.tid));

                            let mut events = [0u64; 2];
                            let switch_result = sys_umcg_ctl(
                                0,
                                UmcgCmd::CtxSwitch,
                                worker.tid,
                                0,
                                Some(&mut events),
                                2
                            );

                            if switch_result == 0 {
                                log_with_timestamp(&format!("Server {}: Context switch to worker {} successful",
                                                            self.id, worker.id));
                            } else {
                                log_with_timestamp(&format!("Server {}: Context switch to worker {} failed: {}",
                                                            self.id, worker.id, switch_result));
                            }

                            self.worker_pool.return_worker(worker, tx);
                            true
                        } else {
                            log_with_timestamp(&format!("Server {}: Worker {} not ready for tasks", self.id, worker.id));
                            self.worker_pool.return_worker(worker, tx);
                            task_queue.enqueue(task);
                            false
                        }
                    } else {
                        log_with_timestamp(&format!("Server {}: No workers available, re-queueing task", self.id));
                        task_queue.enqueue(task);
                        false
                    }
                }
                TaskState::Running { worker_tid, preempted: true, .. } => {
                    log_with_timestamp(&format!("Server {}: Resuming preempted worker {}", self.id, worker_tid));
                    let mut events = [0u64; 2];
                    let switch_result = sys_umcg_ctl(
                        0,
                        UmcgCmd::CtxSwitch,
                        *worker_tid,
                        0,
                        Some(&mut events),
                        2
                    );
                    log_with_timestamp(&format!("Server {}: Resume result: {}", self.id, switch_result));
                    true
                }
                _ => {
                    log_with_timestamp(&format!("Server {}: Task in unexpected state", self.id));
                    false
                }
            }
        } else {
            log_with_timestamp(&format!("Server {}: No tasks to process", self.id));
            false
        }
    }

    fn process_event(&self, event: u64) {
        log_with_timestamp(&format!("Server {}: Raw event value: 0x{:x}", self.id, event));
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        log_with_timestamp(&format!("Server {}: Decoded - type: {}, worker_tid: {}",
                                    self.id, event_type, worker_tid));

        match event_type {
            1 => { // BLOCK
                log_with_timestamp(&format!("Server {}: Worker {} blocked", self.id, worker_tid));
                let mut states = self.worker_states.lock().unwrap();
                if let Some(state) = states.get(&worker_tid) {
                    log_with_timestamp(&format!("Server {}: Worker {} state transition {:?} -> BLOCKED",
                                                self.id, worker_tid, state.status));
                }
                states.entry(worker_tid)
                    .and_modify(|state| {
                        state.status = WorkerStatus::Blocked;
                        log_with_timestamp(&format!("Server {}: Updated worker {} status to Blocked",
                                                    self.id, worker_tid));
                    })
                    .or_insert_with(|| {
                        let state = WorkerState::new(worker_tid);
                        log_with_timestamp(&format!("Server {}: Created new state for blocked worker {}",
                                                    self.id, worker_tid));
                        state
                    });
                drop(states);
                self.process_next_task();  // Try to schedule another task while this one is blocked
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
                log_with_timestamp(&format!("Server {}: Worker {} woke up", self.id, worker_tid));
                let mut states = self.worker_states.lock().unwrap();

                // If worker doesn't exist, this is their initial WAKE after registration
                if !states.contains_key(&worker_tid) {
                    log_with_timestamp(&format!("Server {}: Initial wake for worker {}", self.id, worker_tid));
                    let mut state = WorkerState::new(worker_tid);
                    state.status = WorkerStatus::Waiting;  // Initial state is Waiting
                    states.insert(worker_tid, state);
                    drop(states);
                    self.process_next_task();  // Try to assign initial task
                } else {
                    // Check current state to determine wake type
                    let current_state = states.get(&worker_tid)
                        .map(|s| s.status.clone())
                        .unwrap_or(WorkerStatus::Running);

                    match current_state {
                        WorkerStatus::Running => {
                            // This is a wake after task completion - treat as WAIT
                            log_with_timestamp(&format!("Server {}: Worker {} completed task, ready for new work",
                                                        self.id, worker_tid));
                            states.entry(worker_tid)
                                .and_modify(|state| {
                                    log_with_timestamp(&format!("Server {}: Worker {} state transition Running -> Waiting",
                                                                self.id, worker_tid));
                                    state.status = WorkerStatus::Waiting;
                                    state.current_task = None;
                                });
                            drop(states);
                            self.process_next_task();  // Try to assign new task
                        },
                        WorkerStatus::Blocked => {
                            // This is a wake after being blocked - continue current task
                            log_with_timestamp(&format!("Server {}: Worker {} unblocked, continuing task",
                                                        self.id, worker_tid));
                            states.entry(worker_tid)
                                .and_modify(|state| {
                                    log_with_timestamp(&format!("Server {}: Worker {} state transition Blocked -> Running",
                                                                self.id, worker_tid));
                                    state.status = WorkerStatus::Running;
                                });
                            drop(states);

                            // Context switch back to let worker continue its task
                            let mut events = [0u64; 2];
                            let switch_result = sys_umcg_ctl(
                                0,
                                UmcgCmd::CtxSwitch,
                                worker_tid,
                                0,
                                Some(&mut events),
                                2
                            );
                            log_with_timestamp(&format!("Server {}: Resume context switch result: {}",
                                                        self.id, switch_result));
                        },
                        WorkerStatus::Waiting => {
                            // Shouldn't happen - we don't expect wake events from waiting workers
                            log_with_timestamp(&format!("Server {}: Unexpected wake from waiting worker {}",
                                                        self.id, worker_tid));
                        },
                        WorkerStatus::Completed => {
                            // Shouldn't happen - completed workers shouldn't send events
                            log_with_timestamp(&format!("Server {}: Unexpected wake from completed worker {}",
                                                        self.id, worker_tid));
                        }
                    }
                }
            },
            3 => { // WAIT - We might not get these anymore, but keep the handler for completeness
                log_with_timestamp(&format!("Server {}: Worker {} waiting", self.id, worker_tid));
                let mut states = self.worker_states.lock().unwrap();
                if let Some(state) = states.get(&worker_tid) {
                    log_with_timestamp(&format!("Server {}: Worker {} state transition {:?} -> WAITING",
                                                self.id, worker_tid, state.status));
                }
                states.entry(worker_tid)
                    .and_modify(|state| {
                        state.status = WorkerStatus::Waiting;
                        state.current_task = None;
                        log_with_timestamp(&format!("Server {}: Updated worker {} status to Waiting",
                                                    self.id, worker_tid));
                    })
                    .or_insert_with(|| {
                        let mut state = WorkerState::new(worker_tid);
                        state.status = WorkerStatus::Waiting;
                        log_with_timestamp(&format!("Server {}: Created new state for waiting worker {}",
                                                    self.id, worker_tid));
                        state
                    });
                drop(states);
                self.process_next_task();
            },
            4 => { // EXIT
                log_with_timestamp(&format!("Server {}: Worker {} exited", self.id, worker_tid));
                let mut states = self.worker_states.lock().unwrap();
                if let Some(state) = states.get_mut(&worker_tid) {
                    log_with_timestamp(&format!("Server {}: Worker {} state transition {:?} -> Completed",
                                                self.id, worker_tid, state.status));
                    state.status = WorkerStatus::Completed;
                    log_with_timestamp(&format!("Server {}: Updated worker {} status to Completed",
                                                self.id, worker_tid));
                }
            },
            _ => log_with_timestamp(&format!("Server {}: Unknown event {} from worker {}",
                                             self.id, event_type, worker_tid)),
        }
    }

    fn start(self) -> JoinHandle<()> {
        thread::spawn(move || {
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

fn get_thread_id() -> pid_t {
    unsafe { syscall(SYS_gettid) as pid_t }
}

fn sys_umcg_ctl(
    flags: u64,
    cmd: UmcgCmd,
    next_tid: pid_t,
    abs_timeout: u64,
    events: Option<&mut [u64]>,
    event_sz: i32,
) -> i32 {
    log_with_timestamp(&format!("UMCG syscall - cmd: {:?}, tid: {}, flags: {}", cmd, next_tid, flags));
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
    log_with_timestamp(&format!("UMCG syscall result: {}", result));
    result
}

fn umcg_wait_retry(worker_id: u64, mut events_buf: Option<&mut [u64]>, event_sz: i32) -> i32 {
    let mut flags = 0;
    loop {
        log_with_timestamp(&format!("UMCG wait retry - worker: {}, flags: {}", worker_id, flags));
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