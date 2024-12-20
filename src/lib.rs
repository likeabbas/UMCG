use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::collections::{VecDeque, HashMap};

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
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            server_count: 1,
        }
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
            for i in 0..server_count {
                servers.push(Server::new(i, executor_clone.clone()));
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

struct TaskTracker {
    task_states: HashMap<i32, TaskState>,
}

impl TaskTracker {
    fn new() -> Self {
        Self {
            task_states: HashMap::new(),
        }
    }

    fn track_new_task(&mut self, worker_tid: i32) {
        self.task_states.insert(worker_tid, TaskState::Running {
            worker_tid,
            start_time: SystemTime::now(),
            preempted: false,
            blocked: false,
            state: WorkerState::Running,
        });
    }

    fn mark_blocked(&mut self, worker_tid: i32) {
        if let Some(TaskState::Running { ref mut blocked, ref mut state, .. }) = self.task_states.get_mut(&worker_tid) {
            *blocked = true;
            *state = WorkerState::Blocked;
        }
    }

    fn mark_unblocked(&mut self, worker_tid: i32) {
        if let Some(TaskState::Running { ref mut blocked, ref mut state, .. }) = self.task_states.get_mut(&worker_tid) {
            *blocked = false;
            *state = WorkerState::Running;
        }
    }

    fn mark_completed(&mut self, worker_tid: i32) {
        if self.task_states.contains_key(&worker_tid) {
            self.task_states.insert(worker_tid, TaskState::Completed);
        }
    }
}

#[derive(Clone)]
struct Server {
    id: usize,
    task_queue: Arc<Mutex<VecDeque<Task>>>,
    task_tracker: Arc<Mutex<TaskTracker>>,
    executor: Arc<Executor>,
    runnable_workers: Arc<Mutex<VecDeque<i32>>>,
    completed_cycles: Arc<Mutex<HashMap<u64, bool>>>,
    workers_count: Arc<AtomicI32>,
    done: Arc<AtomicBool>,
    worker_count: Arc<AtomicI32>,
}

impl Server {
    fn new(id: usize, executor: Arc<Executor>) -> Self {
        Self {
            id,
            task_queue: Arc::new(Mutex::new(VecDeque::new())),
            task_tracker: Arc::new(Mutex::new(TaskTracker::new())),
            executor,
            runnable_workers: Arc::new(Mutex::new(VecDeque::new())),
            completed_cycles: Arc::new(Mutex::new(HashMap::new())),
            workers_count: Arc::new(AtomicI32::new(0)),
            done: Arc::new(AtomicBool::new(false)),
            worker_count: Arc::new(AtomicI32::new(0)),
        }
    }

    fn add_task(&self, task: Task) {
        let mut queue = self.task_queue.lock().unwrap();
        queue.push_back(task);
        log_with_timestamp(&format!("Server {}: Queued new task. Queue size now {}", self.id, queue.len()));
    }

    fn process_event(&self, event: u64) {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;

        match event_type {
            1 => { // BLOCK
                log_with_timestamp(&format!("Server {}: Worker {} blocked", self.id, worker_tid));
                self.task_tracker.lock().unwrap().mark_blocked(worker_tid);
                let mut runnable_workers = self.runnable_workers.lock().unwrap();
                if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid) {
                    runnable_workers.remove(pos);
                }
            },
            2 => { // WAKE
                log_with_timestamp(&format!("Server {}: Worker {} woke up", self.id, worker_tid));
                self.task_tracker.lock().unwrap().mark_unblocked(worker_tid);
                let mut runnable_workers = self.runnable_workers.lock().unwrap();
                if !runnable_workers.contains(&worker_tid) {
                    runnable_workers.push_back(worker_tid);
                }
            },
            3 => { // WAIT
                log_with_timestamp(&format!("Server {}: Worker {} waiting", self.id, worker_tid));
                let mut runnable_workers = self.runnable_workers.lock().unwrap();
                if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid) {
                    runnable_workers.remove(pos);
                    runnable_workers.push_back(worker_tid);
                }
            },
            4 => { // EXIT
                log_with_timestamp(&format!("Server {}: Worker {} exited", self.id, worker_tid));
                self.task_tracker.lock().unwrap().mark_completed(worker_tid);
                let mut runnable_workers = self.runnable_workers.lock().unwrap();
                if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid) {
                    runnable_workers.remove(pos);
                }
                self.completed_cycles.lock().unwrap().insert(worker_tid as u64, true);
                self.worker_count.fetch_sub(1, Ordering::SeqCst);
            },
            _ => log_with_timestamp(&format!("Server {}: Unknown event {} from worker {}",
                                             self.id, event_type, worker_tid)),
        }
    }

    fn start(self) -> JoinHandle<()> {
        thread::spawn(move || {
            log_with_timestamp(&format!("Server {}: Starting up", self.id));
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

            let mut worker_handles = Vec::new();
            let mut next_worker_id = 0;

            while !self.done.load(Ordering::Relaxed) {
                // Process new tasks
                if let Some(task) = self.task_queue.lock().unwrap().pop_front() {
                    let worker_id = next_worker_id;
                    next_worker_id += 1;
                    log_with_timestamp(&format!("Server {}: Creating worker {} for task", self.id, worker_id));

                    let (tx, rx) = channel();
                    tx.send(task).unwrap();

                    let worker = Worker::new(
                        worker_id,
                        rx,
                        self.executor.clone(),
                    );

                    self.worker_count.fetch_add(1, Ordering::SeqCst);
                    worker_handles.push(worker.start(
                        self.workers_count.clone(),
                        self.done.clone(),
                    ));
                }

                // Process worker events
                let mut events = [0u64; 6];
                let ret = if let Some(&next_worker) = self.runnable_workers.lock().unwrap().front() {
                    log_with_timestamp(&format!("Server {}: Context switching to worker {}", self.id, next_worker));
                    sys_umcg_ctl(0, UmcgCmd::CtxSwitch, next_worker, 0, Some(&mut events), 6)
                } else {
                    log_with_timestamp(&format!("Server {}: Waiting for worker events...", self.id));
                    umcg_wait_retry(0, Some(&mut events), 6)
                };

                if ret != 0 {
                    eprintln!("Server {} loop error", self.id);
                    break;
                }

                for &event in events.iter().take_while(|&&e| e != 0) {
                    self.process_event(event);
                }

                // Check if server should exit
                if self.worker_count.load(Ordering::SeqCst) == 0
                    && self.task_queue.lock().unwrap().is_empty() {
                    break;
                }
            }

            // Clean shutdown
            self.done.store(true, Ordering::Relaxed);
            for handle in worker_handles {
                handle.join().unwrap();
            }

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
                        thread::sleep(Duration::from_millis(100));
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    println!("[{:>3}.{:03}] {}",
             now.as_secs() % 1000,
             now.subsec_millis(),
             msg);
}

pub fn run_dynamic_task_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 1,
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
        thread::sleep(Duration::from_millis(10));
    }

    log_with_timestamp("All initial tasks submitted, starting execution...");
    executor.start();

    thread::sleep(Duration::from_secs(10));
    0
}

pub fn run_multi_server_demo() -> i32 {
    let config = ExecutorConfig {
        server_count: 3,
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
        thread::sleep(Duration::from_millis(10));
    }

    log_with_timestamp("All tasks submitted, starting servers...");
    executor.start();

    thread::sleep(Duration::from_secs(15));
    0
}