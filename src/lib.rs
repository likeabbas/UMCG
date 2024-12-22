use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver};
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
enum WorkerState {
    Running,
    Blocked,
    Preempted,
    Completed,
}

#[derive(Debug)]
enum TaskState {
    Pending(Box<dyn FnOnce(&TaskHandle) + Send>),
    Running {
        worker_tid: i32,
        start_time: SystemTime,
        preempted: bool,
        blocked: bool,
        state: WorkerState,
    },
    Completed,
}

enum Task {
    Function(Box<dyn FnOnce(&TaskHandle) + Send>),
    Shutdown,
}

struct TaskEntry {
    id: Uuid,  // Unique task identifier
    state: TaskState,
    priority: u32,  // For future preemption support
    enqueue_time: SystemTime,
}

impl TaskEntry {
    fn new(task: Box<dyn FnOnce(&TaskHandle) + Send>) -> Self {
        Self {
            id: Uuid::new_v4(),
            state: TaskState::Pending(task),
            priority: 0,  // Default priority
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
        Self {
            pending: VecDeque::new(),
            preempted: VecDeque::new(),
            in_progress: HashMap::new(),
            mutex: Mutex::new(()),
        }
    }

    fn enqueue(&mut self, task: TaskEntry) {
        let _guard = self.mutex.lock().unwrap();
        match &task.state {
            TaskState::Pending(_) => self.pending.push_back(task),
            TaskState::Running { preempted: true, .. } => self.preempted.push_back(task),
            TaskState::Running { .. } => {
                self.in_progress.insert(task.id, task);
            }
            TaskState::Completed => {
                log_with_timestamp(&format!("Warning: Attempting to enqueue completed task {}", task.id));
            }
        }
    }

    fn get_next_task(&mut self) -> Option<TaskEntry> {
        let _guard = self.mutex.lock().unwrap();
        // First try to get a preempted task that needs resuming
        self.preempted
            .pop_front()
            .or_else(|| self.pending.pop_front())
    }

    fn mark_in_progress(&mut self, task_id: Uuid, worker_tid: i32) {
        let _guard = self.mutex.lock().unwrap();
        if let Some(task) = self.in_progress.get_mut(&task_id) {
            if let TaskState::Pending(_) = task.state {
                task.state = TaskState::Running {
                    worker_tid,
                    start_time: SystemTime::now(),
                    preempted: false,
                    blocked: false,
                    state: WorkerState::Running,
                };
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

struct WorkerPool {
    available_workers: Arc<Mutex<VecDeque<Worker>>>,
    total_workers: usize,
}

impl WorkerPool {
    fn new(size: usize, executor: Arc<Executor>) -> Self {
        let mut workers = VecDeque::with_capacity(size);
        for id in 0..size {
            let (tx, rx) = channel();
            workers.push_back(Worker::new(id, rx, executor.clone()));
        }
        Self {
            available_workers: Arc::new(Mutex::new(workers)),
            total_workers: size,
        }
    }

    fn get_worker(&self) -> Option<Worker> {
        self.available_workers.lock().unwrap().pop_front()
    }

    fn return_worker(&self, worker: Worker) {
        self.available_workers.lock().unwrap().push_back(worker);
    }
}

struct Executor {
    servers: Mutex<Vec<Server>>,
    next_server: AtomicUsize,
    config: ExecutorConfig,
}

impl Executor {
    fn new(config: ExecutorConfig) -> Arc<Self> {
        let server_count = config.server_count;
        let executor = Arc::new(Self {
            servers: Mutex::new(Vec::with_capacity(server_count)),
            next_server: AtomicUsize::new(0),
            config,
        });

        let executor_clone = executor.clone();
        {
            let mut servers = executor_clone.servers.lock().unwrap();
            let worker_pool = Arc::new(WorkerPool::new(
                config.worker_count,
                executor_clone.clone()
            ));

            for i in 0..server_count {
                servers.push(Server::new(i, executor_clone.clone(), worker_pool.clone()));
            }
        }

        executor
    }

    fn submit(&self, task: Box<dyn FnOnce(&TaskHandle) + Send>) {
        log_with_timestamp("Queuing new task");

        let server_idx = self.next_server.fetch_add(1, Ordering::Relaxed) % self.config.server_count;

        let servers = self.servers.lock().unwrap();
        if let Some(server) = servers.get(server_idx) {
            server.add_task(Task::Function(task));
            log_with_timestamp(&format!("Task assigned to server {}", server_idx));
        }
    }

    fn start(&self) {
        let servers = self.servers.lock().unwrap();
        for server in servers.iter() {
            server.clone().start();
        }
    }
}

#[derive(Clone)]
struct Server {
    id: usize,
    task_queue: Arc<Mutex<TaskQueue>>,
    worker_pool: Arc<WorkerPool>,
    executor: Arc<Executor>,
    completed_cycles: Arc<Mutex<HashMap<Uuid, bool>>>,
    workers_count: Arc<AtomicI32>,
    done: Arc<AtomicBool>,
}

impl Server {
    fn new(id: usize, executor: Arc<Executor>, worker_pool: Arc<WorkerPool>) -> Self {
        Self {
            id,
            task_queue: Arc::new(Mutex::new(TaskQueue::new())),
            worker_pool,
            executor,
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            workers_count: Arc::new(AtomicI32::new(0)),
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn add_task(&self, task: Task) {
        match task {
            Task::Function(f) => {
                let task_entry = TaskEntry::new(f);
                let mut queue = self.task_queue.lock().unwrap();
                queue.enqueue(task_entry);
                log_with_timestamp(&format!("Server {}: Queued new task", self.id));
            }
            Task::Shutdown => {
                self.done.store(true, Ordering::Relaxed);
            }
        }
    }

    fn process_next_task(&self) -> bool {
        let mut task_queue = self.task_queue.lock().unwrap();

        if let Some(task) = task_queue.get_next_task() {
            match &task.state {
                TaskState::Pending(_) => {
                    // Try to get an available worker
                    if let Some(worker) = self.worker_pool.get_worker() {
                        task_queue.mark_in_progress(task.id, worker.tid);
                        worker.assign_task(task);
                        true
                    } else {
                        // No worker available, put task back
                        task_queue.enqueue(task);
                        false
                    }
                }
                TaskState::Running { worker_tid, preempted: true, .. } => {
                    // Context switch back to the worker
                    let mut events = [0u64; 2];
                    sys_umcg_ctl(
                        0,
                        UmcgCmd::CtxSwitch,
                        *worker_tid,
                        0,
                        Some(&mut events),
                        2
                    );
                    true
                }
                _ => false
            }
        } else {
            false
        }
    }

    fn process_event(&self, event: u64) {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        match event_type {
            1 => { // BLOCK
                log_with_timestamp(&format!("Server {}: Worker {} blocked", self.id, worker_tid));
                // Handle blocked worker
                if let Some(worker) = self.worker_pool.get_worker() {
                    // Return worker to pool as it's now blocked
                    self.worker_pool.return_worker(worker);
                }
            },
            2 => { // WAKE
                log_with_timestamp(&format!("Server {}: Worker {} woke up", self.id, worker_tid));
                // Worker is now available again
                if let Some(worker) = self.worker_pool.get_worker() {
                    // Process next task with the newly available worker
                    self.process_next_task();
                }
            },
            3 => { // WAIT
                log_with_timestamp(&format!("Server {}: Worker {} waiting", self.id, worker_tid));
                // Similar to BLOCK but for voluntary yields
                if let Some(worker) = self.worker_pool.get_worker() {
                    self.worker_pool.return_worker(worker);
                }
            },
            4 => { // EXIT
                log_with_timestamp(&format!("Server {}: Worker {} exited", self.id, worker_tid));
                // Clean up any resources associated with the exited worker
                self.workers_count.fetch_sub(1, Ordering::SeqCst);
            },
            _ => log_with_timestamp(&format!("Server {}: Unknown event {} from worker {}",
                                             self.id, event_type, worker_tid)),
        }
    }

    fn start(self) -> JoinHandle<()> {
        thread::spawn(move || {
            log_with_timestamp(&format!("Server {}: Starting up", self.id));
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

            while !self.done.load(Ordering::Relaxed) {
                // Process worker events
                let mut events = [0u64; 6];
                let ret = umcg_wait_retry(0, Some(&mut events), 6);

                if ret != 0 {
                    eprintln!("Server {} loop error", self.id);
                    break;
                }

                for &event in events.iter().take_while(|&&e| e != 0) {
                    self.process_event(event);
                }

                // Try to process next task
                self.process_next_task();
            }

            // Clean shutdown
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
            log_with_timestamp(&format!("Server {}: Shutdown complete", self.id));
        })
    }
}

struct Worker {
    id: usize,
    tid: pid_t,
    task_rx: Receiver<Task>,
    handle: TaskHandle,
}

impl Worker {
    fn new(id: usize, task_rx: Receiver<Task>, executor: Arc<Executor>) -> Self {
        Self {
            id,
            tid: 0,
            task_rx,
            handle: TaskHandle { executor },
        }
    }

    fn assign_task(&self, task: TaskEntry) {
        // Implementation for assigning task to worker
        // This would need to handle both new tasks and resuming preempted tasks
    }

    fn start(mut self, workers_count: Arc<AtomicI32>, done: Arc<AtomicBool>) -> JoinHandle<()> {
        thread::spawn(move || {
            self.tid = get_thread_id();
            log_with_timestamp(&format!("Worker {}: Initialized with tid {}", self.id, self.tid));

            assert_eq!(
                sys_umcg_ctl(
                    0,
                    UmcgCmd::RegisterWorker,
                    0,
                    (self.tid as u64) << UMCG_WORKER_ID_SHIFT,
                    None,
                    0
                ),
                0
            );

            workers_count.fetch_add(1, Ordering::Relaxed);

            while !done.load(Ordering::Relaxed) {
                match self.task_rx.try_recv() {
                    Ok(Task::Function(task)) => {
                        log_with_timestamp(&format!("Worker {}: Starting task execution", self.id));
                        task(&self.handle);
                        log_with_timestamp(&format!("Worker {}: Completed task execution", self.id));
                    }
                    Ok(Task::Shutdown) => {
                        log_with_timestamp(&format!("Worker {}: Received shutdown signal", self.id));
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        if done.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break;
                    }
                }
            }

            log_with_timestamp(&format!("Worker {}: Shutting down", self.id));
            workers_count.fetch_sub(1, Ordering::Relaxed);
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
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
    unsafe {
        syscall(
            SYS_UMCG_CTL,
            flags as i64,
            cmd as i64,
            next_tid as i64,
            abs_timeout as i64,
            events.map_or(std::ptr::null_mut(), |e| e.as_mut_ptr()) as i64,
            event_sz as i64,
        ) as i32
    }
}

fn umcg_wait_retry(worker_id: u64, mut events_buf: Option<&mut [u64]>, event_sz: i32) -> i32 {
    let mut flags = 0;
    loop {
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

// Example usage functions
pub fn run_dynamic_task_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 1,
        worker_count: 3,
    };
    let executor = Executor::new(config);

    log_with_timestamp("Submitting initial tasks...");

    for i in 0..3 {
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

    log_with_timestamp("All initial tasks submitted, starting execution...");
    executor.start();

    thread::sleep(Duration::from_secs(10));
    0
}

pub fn run_multi_server_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 3,
        worker_count: 3,
    };
    let executor = Executor::new(config);

    log_with_timestamp("Submitting tasks to multiple servers...");

    for i in 0..9 {
        let task = move |handle: &TaskHandle| {
            log_with_timestamp(&format!("Task {}: Starting execution", i));
            thread::sleep(Duration::from_secs(1));

            let parent_id = i;
            handle.submit(move |_| {
                log_with_timestamp(&format!("Child of task {}: Starting work", parent_id));
                thread::sleep(Duration::from_millis(500));
                log_with_timestamp(&format!("Child of task {}: Completed", parent_id));
            });

            log_with_timestamp(&format!("Task {}: Completed", i));
        };

        executor.submit(Box::new(task));
    }

    log_with_timestamp("All tasks submitted, starting servers...");
    executor.start();

    thread::sleep(Duration::from_secs(15));
    0
}