/*
This is our primary file where we're trying to get UMCG working with
a thread pool, and multiple servers with their own workers.
 */

#![allow(warnings)]

mod umcg_base;
mod task_stats;
mod attempt2;

use crossbeam_queue::ArrayQueue;
use libc::{self, pid_t, syscall, SYS_gettid, EINTR};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;
use log::error;
use task_stats::TaskStats;
use umcg_base::{UmcgCmd, UmcgEventType, DEBUG_LOGGING, EVENT_BUFFER_SIZE, SYS_UMCG_CTL, UMCG_WAIT_FLAG_INTERRUPTED, UMCG_WORKER_EVENT_MASK, UMCG_WORKER_ID_SHIFT, WORKER_REGISTRATION_TIMEOUT_MS};

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

pub fn run_basic_worker_test() -> i32 {
    debug!("Running basic worker test...");
    attempt2::test_basic_worker();
    0
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
}

impl WorkerState {
    fn new(tid: pid_t, server_id: usize) -> Self {  // Add server_id parameter
        info!("Creating new WorkerState for tid {} on server {}", tid, server_id);
        Self {
            tid,
            status: WorkerStatus::Waiting,
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
        } else {
            error!("WorkerPool: No worker found with tid {}", tid);
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
            if let Err(e) = umcg_base::set_cpu_affinity(self.cpu_id) {
                debug!("Could not set CPU affinity for worker {} to CPU {}: {}",
                    id, self.cpu_id, e);
            }

            self.tid = umcg_base::get_thread_id();
            info!("Worker {}: Initialized with tid {}", self.id, self.tid);

            tid_tx.send(self.tid).expect("Failed to send worker tid");

            let worker_id = (self.tid as u64) << UMCG_WORKER_ID_SHIFT;

            // register workers with just the one server
            // let worker_id = (((self.server_id as u64) << 32) | (self.tid as u64)) << UMCG_WORKER_ID_SHIFT;
            debug!("Worker {}: Registering with UMCG (worker_id: {}) for server {}",
                self.id, worker_id, self.server_id);

            // Register with UMCG and specific server
            let reg_result = umcg_base::sys_umcg_ctl(
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
                        let wait_result = umcg_base::sys_umcg_ctl(
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
                        let wait_result = umcg_base::sys_umcg_ctl(
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
            let unreg_result = umcg_base::sys_umcg_ctl(
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

            let ret = umcg_base::umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
            if ret != 0 {
                return Err(ServerError::WorkerRegistrationFailed {
                    worker_id,
                    error: ret
                });
            }

            let event = events[0];
            let event_type = event & UMCG_WORKER_EVENT_MASK;
            // Maybe here we need to do a Wake and then a Wait
            // context switch back to the worker after it Wakes so it can go into the Wait state
            if event_type == UmcgEventType::Wake as u64 {
                return Ok(event);
            } else {
                debug!("Server {}: Unexpected event {} during worker {} registration",
                    self.id, event_type, worker_id);
            }
        }
    }
    // fn wait_for_worker_registration(&self, worker_id: usize) -> Result<u64, ServerError> {
    //     let start = SystemTime::now();
    //     let timeout = Duration::from_millis(WORKER_REGISTRATION_TIMEOUT_MS);
    //     let mut events = [0u64; EVENT_BUFFER_SIZE];
    //
    //     // First wait for Wake event
    //     loop {
    //         if start.elapsed().unwrap() > timeout {
    //             return Err(ServerError::WorkerRegistrationTimeout { worker_id });
    //         }
    //
    //         let ret = umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
    //         if ret != 0 {
    //             return Err(ServerError::WorkerRegistrationFailed {
    //                 worker_id,
    //                 error: ret
    //             });
    //         }
    //
    //         let event = events[0];
    //         let event_type = event & UMCG_WORKER_EVENT_MASK;
    //
    //         if event_type == UmcgEventType::Wake as u64 {
    //             // Got Wake, now context switch to let worker enter Wait state
    //             let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;
    //
    //             let switch_result = sys_umcg_ctl(
    //                 0,
    //                 UmcgCmd::CtxSwitch,
    //                 worker_tid,
    //                 0,
    //                 Some(&mut events),
    //                 EVENT_BUFFER_SIZE as i32
    //             );
    //
    //             if switch_result != 0 {
    //                 return Err(ServerError::SystemError(std::io::Error::last_os_error()));
    //             }
    //
    //             // Now wait for Wait event
    //             let ret = umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
    //             if ret != 0 {
    //                 return Err(ServerError::WorkerRegistrationFailed {
    //                     worker_id,
    //                     error: ret
    //                 });
    //             }
    //
    //             let wait_event = events[0];
    //             let wait_event_type = wait_event & UMCG_WORKER_EVENT_MASK;
    //
    //             if wait_event_type == UmcgEventType::Wait as u64 {
    //                 return Ok(event); // Return original Wake event
    //             }
    //         }
    //
    //         debug!("Server {}: Unexpected event {} during worker {} registration",
    //         self.id, event_type, worker_id);
    //     }
    // }

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
                /*
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
                            // debug!("!!!!!!!!!!! Wake but worker status was in Waiting - This shouldn't happen !!!!");
                            // debug!("!!!!!!!!!! WAKE: This is a Wait->Wake from klib (worker already Waiting) !!!!!!!!!!");
                            // let mut task_queue = self.task_queue.lock().unwrap();
                            // if let Some(task) = task_queue.get_next_task() {
                            //     debug!("!!!!!!!!!! WAKE: Found task for waiting worker !!!!!!!!!!");
                            //     self.worker_pool.update_worker_status(
                            //         worker_tid,
                            //         WorkerStatus::Running,
                            //         Some(task.id)
                            //     );
                            //     task_queue.mark_in_progress(task.id, worker_tid);
                            //     drop(task_queue);
                            //
                            //     if let Some((worker, tx)) = self.worker_pool.workers.lock().unwrap()
                            //         .get(&worker_tid)
                            //         .map(|w| (w.clone(), w.tx.clone()))
                            //     {
                            //         match worker.assign_task(task, &tx) {
                            //             Ok(()) => {
                            //                 debug!("!!!!!!!!!! WAKE: Assigned task to waiting worker !!!!!!!!!!");
                            //                 if let Err(e) = self.context_switch_worker(worker_tid) {
                            //                     error!("!!!!!!!!!! WAKE: Context switch failed for waiting worker !!!!!!!!!!");
                            //                     self.worker_pool.update_worker_status(
                            //                         worker_tid,
                            //                         WorkerStatus::Waiting,
                            //                         None
                            //                     );
                            //                 }
                            //             }
                            //             Err(failed_task) => {
                            //                 debug!("!!!!!!!!!! WAKE: Task assignment failed for waiting worker !!!!!!!!!!");
                            //                 let mut task_queue = self.task_queue.lock().unwrap();
                            //                 task_queue.enqueue(failed_task);
                            //                 self.worker_pool.update_worker_status(
                            //                     worker_tid,
                            //                     WorkerStatus::Waiting,
                            //                     None
                            //                 );
                            //             }
                            //         }
                            //     }
                            // }
                        }
                        _ => {
                            debug!("!!!!!!!!!!! This shouldn't happen !!!!");
                    //         debug!("!!!!!!!!!! WAKE: This is likely an initial registration WAKE (worker status: {:?}) !!!!!!!!!!",
                    // current_status);
                            // debug!("Server {}: Worker {} in initial state", self.id, worker_tid);
                            // self.worker_pool.update_worker_status(
                            //     worker_tid,
                            //     WorkerStatus::Waiting,
                            //     None
                            // );
                        }
                    }
                } else {
                    debug!("!!! NO FUCKING WORKER STATE FOR WORKER {}!!", worker_tid);
                }
            },
            3 => { // WAIT
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
        if let Err(e) = umcg_base::set_cpu_affinity(self.cpu_id) {
            return Err(ServerError::SystemError(e));
        }

        info!("Server {}: Starting up", self.id);

        let reg_result = umcg_base::sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0);
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
            let ret = umcg_base::sys_umcg_ctl(
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
        let unreg_result = umcg_base::sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
        if unreg_result != 0 {
            error!("Server {}: UMCG unregister failed: {}", self.id, unreg_result);
        }
    }

    fn context_switch_worker(&self, worker_tid: pid_t) -> Result<(), ServerError> {
        let mut events = [0u64; 2];
        debug!("Server {}: Context switching to worker {}", self.id, worker_tid);

        let switch_result = umcg_base::sys_umcg_ctl(
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

    debug!("Starting executor...");
    executor.start();

    thread::sleep(Duration::from_millis(100));

    debug!("Submitting tasks to multiple servers...");

    for i in 0..9 {
        let task = move |handle: &TaskHandle| {
            debug!("Task {}: Starting execution", i);
            thread::sleep(Duration::from_secs(1));

            let parent_id = i;
            handle.submit(move |_| {
                debug!("Child task of initial task {}: Starting work", parent_id);
                thread::sleep(Duration::from_millis(500));
                log_with_timestamp(&format!("Child task of initial task {}: Completed", parent_id));
            });

            debug!("Task {}: Completed", i);
        };

        executor.submit(Box::new(task));
    }

    log_with_timestamp("All tasks submitted, waiting for completion...");
    thread::sleep(Duration::from_secs(15));
    0
}


/*

CLAUDE READ THIS - IT'S VERY IMPORTANT
So our code for the run_dynamic_task_demo works sometimes, but then fails other times. There's either a deadlock or something
or something happening but I"m unable to trace it. From 1 different runs we can see it succeeds once,
then another run where none of the tasks succeed.

// Working case
➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[897.538] Creating new Executor
[897.552] Creating Server 0 on CPU 0
[897.561] Creating Server 0
[897.564] Creating new TaskQueue
[897.571] Creating WorkerPool with capacity for 3 workers
[897.575] Starting executor...
[897.576] Executor: Starting servers
[897.602] Starting server 0 initialization
[897.604] Server 0 ready for tasks
[897.620] Successfully set CPU affinity to 0
[897.636] Server 0: Starting up
[897.638] All 1 servers started and ready
[897.643] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[897.646] UMCG syscall result: 0
[897.647] Server 0: UMCG registration complete
[897.648] Server 0: Initializing workers
[897.649] Server 0: Initializing worker 0
[897.652] Starting WorkerThread 0 for server 0
[897.662] Successfully set CPU affinity to 0
[897.664] Worker 0: Initialized with tid 4
[897.666] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[897.670] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[897.669] WorkerThread 0 started with tid 4 for server 0
[897.673] Creating new Worker 0 for server 0
[897.673] WorkerPool: Adding worker 0 with tid 4
[897.676] WorkerPool: Updating worker 0 status from Initializing to Registering
[897.677] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[897.678] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[897.679] UMCG syscall result: 0
[897.680] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[897.681] Worker 0: Registering worker event 130
[897.681] WorkerPool: Updating worker 0 status from Registering to Waiting
[897.682] Server 0: Worker 0 initialized successfully
[897.683] Server 0: Initializing worker 1
[897.685] Starting WorkerThread 1 for server 0
[897.690] Successfully set CPU affinity to 0
[897.693] Worker 1: Initialized with tid 5
[897.698] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[897.701] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[897.702] Submitting initial tasks...
[897.703] WorkerThread 1 started with tid 5 for server 0
[897.703] !!!! Submitting initial task 0 !!!!
[897.703] Creating new Worker 1 for server 0
[897.708] Executor: Submitting new task
[897.709] WorkerPool: Adding worker 1 with tid 5
[897.710] WorkerPool: Updating worker 1 status from Initializing to Registering
[897.711] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[897.711] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[897.712] UMCG syscall result: 0
[897.713] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[897.713] Worker 0: Registering worker event 162
[897.714] WorkerPool: Updating worker 1 status from Registering to Waiting
[897.715] Server 0: Worker 1 initialized successfully
[897.716] Server 0: Initializing worker 2
[897.717] Starting WorkerThread 2 for server 0
[897.721] Successfully set CPU affinity to 0
[897.721] Worker 2: Initialized with tid 6
[897.724] Registered task 226ee4dc-2227-442a-8c41-878a87017aa6, total tasks: 1
[897.724] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[897.729] Executor: Adding task 226ee4dc-2227-442a-8c41-878a87017aa6 to server 0 (has 2 workers)
[897.730] Server 0: Adding new task
[897.735] Creating new TaskEntry with ID: 88b38062-7f85-45bd-8d34-f9d3e145d472
[897.727] WorkerThread 2 started with tid 6 for server 0
[897.741] TaskQueue: Enqueueing task 88b38062-7f85-45bd-8d34-f9d3e145d472
[897.741] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[897.741] Creating new Worker 2 for server 0
[897.742] WorkerPool: Adding worker 2 with tid 6
[897.743] WorkerPool: Updating worker 2 status from Initializing to Registering
[897.743] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[897.743] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[897.745] Server 0: Task queued
[897.743] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[897.746] UMCG syscall result: 0
[897.746] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[897.747] Worker 0: Registering worker event 194
[897.749] WorkerPool: Updating worker 2 status from Registering to Waiting
[897.750] Server 0: Worker 2 initialized successfully
[897.750] Server 0: All workers initialized
[897.746] Task 226ee4dc-2227-442a-8c41-878a87017aa6 assigned to server 0
[897.751] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[897.752] !!!! Submitting initial task 1 !!!!
[897.752] Server 0: Attempting to schedule tasks...
[897.753] Server 0: Looking for waiting workers among 3 workers
[897.754] Executor: Submitting new task
[897.754] Server 0: Found waiting worker 2 with status Waiting
[897.756] Registered task f186f0f6-85cd-44f8-8f9b-26fe4d76180d, total tasks: 2
[897.756] TaskQueue: Attempting to get next task
[897.757] Executor: Adding task f186f0f6-85cd-44f8-8f9b-26fe4d76180d to server 0 (has 3 workers)
[897.757] Server 0: Adding new task
[897.757] TaskQueue: Retrieved task 88b38062-7f85-45bd-8d34-f9d3e145d472
[897.758] Creating new TaskEntry with ID: 5d61fd6a-29dd-4b6b-addf-121ccead0507
[897.758] Server 0: Assigning task to worker 2
[897.759] WorkerPool: Updating worker 2 status from Waiting to Running
[897.760] TaskQueue: Marking task 88b38062-7f85-45bd-8d34-f9d3e145d472 as in progress with worker 6
[897.761] Worker 2: Starting task assignment
[897.761] TaskQueue: Enqueueing task 5d61fd6a-29dd-4b6b-addf-121ccead0507
[897.762] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[897.763] Server 0: Task queued
[897.763] Worker 2: Task sent successfully
[897.764] Server 0: Context switching to worker 6
[897.764] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[897.764] Task f186f0f6-85cd-44f8-8f9b-26fe4d76180d assigned to server 0
[897.765] !!!! Submitting initial task 2 !!!!
[897.765] Executor: Submitting new task
[897.768] Registered task 56861260-d2d2-4243-9d9d-cfe6f072a223, total tasks: 3
[897.768] Executor: Adding task 56861260-d2d2-4243-9d9d-cfe6f072a223 to server 0 (has 3 workers)
[897.768] Server 0: Adding new task
[897.769] Creating new TaskEntry with ID: 229ed386-39b4-4da2-8dea-6010b4a65137
[897.770] TaskQueue: Enqueueing task 229ed386-39b4-4da2-8dea-6010b4a65137
[897.770] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[897.770] UMCG syscall result: 0
[897.774] Worker 2: UMCG registration complete with server 0
[897.774] Worker 2: Entering task processing loop
[897.776] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[897.771] Server 0: Task queued
[897.777] !!!! Initial task 0: STARTING task !!!!
[897.778] Task 56861260-d2d2-4243-9d9d-cfe6f072a223 assigned to server 0
[897.778] !!!! Initial task 0: ABOUT TO SLEEP !!!!
[897.778] !!!! Submitting initial task 3 !!!!
[897.778] Executor: Submitting new task
[897.781] UMCG syscall result: 0
[897.782] Registered task 306864ca-a621-4f92-810a-8189293ea2b6, total tasks: 4
[897.782] Server 0: Processing event 1 for worker 6
[897.783] Executor: Adding task 306864ca-a621-4f92-810a-8189293ea2b6 to server 0 (has 3 workers)
[897.783] Server 0: Worker 6 blocked
[897.783] Server 0: Adding new task
[897.783] Creating new TaskEntry with ID: 0ae148df-b30c-4d85-9e58-f72a538bf38b
[897.784] TaskQueue: Enqueueing task 0ae148df-b30c-4d85-9e58-f72a538bf38b
[897.784] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[897.784] Server 0: Task queued
[897.785] Task 306864ca-a621-4f92-810a-8189293ea2b6 assigned to server 0
[897.785] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(88b38062-7f85-45bd-8d34-f9d3e145d472))
[897.785] !!!! Submitting initial task 4 !!!!
[897.786] Executor: Submitting new task
[897.787] Server 0: Successfully switched to worker 6
[897.787] Registered task 7cf19dca-dbf0-492f-b6cc-c88c54dc5312, total tasks: 5
[897.788] Executor: Adding task 7cf19dca-dbf0-492f-b6cc-c88c54dc5312 to server 0 (has 3 workers)
[897.788] Server 0: Adding new task
[897.788] Creating new TaskEntry with ID: 776353e6-d5a4-4de1-b250-c668d737f467
[897.788] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[897.789] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[897.789] TaskQueue: Enqueueing task 776353e6-d5a4-4de1-b250-c668d737f467
[897.789] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[897.790] Server 0: Task queued
[897.790] Task 7cf19dca-dbf0-492f-b6cc-c88c54dc5312 assigned to server 0
[897.790] !!!! Submitting initial task 5 !!!!
[897.790] Executor: Submitting new task
[897.791] Registered task 0b00119c-3679-4305-8456-28aac8bc359d, total tasks: 6
[897.791] Executor: Adding task 0b00119c-3679-4305-8456-28aac8bc359d to server 0 (has 3 workers)
[897.791] Server 0: Adding new task
[897.792] Creating new TaskEntry with ID: 9efa1bbd-3780-47e0-90cf-bc76dd3360b8
[897.792] TaskQueue: Enqueueing task 9efa1bbd-3780-47e0-90cf-bc76dd3360b8
[897.792] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[897.792] Server 0: Task queued
[897.793] Task 0b00119c-3679-4305-8456-28aac8bc359d assigned to server 0
[897.793] All tasks submitted, waiting for completion...
[897.795] Progress: 0/6 tasks completed
[897.906] Progress: 0/6 tasks completed
[898.017] Progress: 0/6 tasks completed
[898.128] Progress: 0/6 tasks completed
[898.231] Progress: 0/6 tasks completed
[898.335] Progress: 0/6 tasks completed
[898.439] Progress: 0/6 tasks completed
[898.543] Progress: 0/6 tasks completed
[898.648] Progress: 0/6 tasks completed
[898.758] Progress: 0/6 tasks completed
en1: assigned FE80::A061:D0FF:FE14:B8D4
[899.795] UMCG syscall result: 0
[899.795] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[899.797] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[899.797] Server 0: Processing event 2 for worker 6
[899.798] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[899.800] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[899.800] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[899.801] Server 0: Worker 6 unblocking
[899.801] WorkerPool: Updating worker 2 status from Blocked to Running
[899.802] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[899.803] Server 0: Context switching to worker 6
[899.803] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[899.805] !!!! Initial task 0: WOKE UP FROM SLEEP !!!!
[899.805] !!!! Initial task 0: PREPARING to spawn child task !!!!
[899.806] TaskHandle: Submitting new task
[899.807] Executor: Submitting new task
[899.807] Registered task db40c964-376a-4a55-9aac-bf41e058d55e, total tasks: 7
[899.808] Executor: Adding task db40c964-376a-4a55-9aac-bf41e058d55e to server 0 (has 3 workers)
[899.809] Server 0: Adding new task
[899.810] Creating new TaskEntry with ID: dad3666a-0f2d-4f78-afe8-c0653db9319b
[899.810] TaskQueue: Enqueueing task dad3666a-0f2d-4f78-afe8-c0653db9319b
[899.812] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[899.812] Server 0: Task queued
[899.812] Task db40c964-376a-4a55-9aac-bf41e058d55e assigned to server 0
[899.814] UMCG syscall result: 0
[899.814] Server 0: Processing event 1 for worker 6
[899.815] Server 0: Worker 6 blocked
[899.816] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(88b38062-7f85-45bd-8d34-f9d3e145d472))
[899.817] Server 0: Attempting to schedule tasks...
[899.817] Server 0: Looking for waiting workers among 3 workers
[899.818] Server 0: Found waiting worker 0 with status Waiting
[899.818] TaskQueue: Attempting to get next task
[899.819] TaskQueue: Retrieved task 5d61fd6a-29dd-4b6b-addf-121ccead0507
[899.820] Server 0: Assigning task to worker 0
[899.820] WorkerPool: Updating worker 0 status from Waiting to Running
[899.820] TaskQueue: Marking task 5d61fd6a-29dd-4b6b-addf-121ccead0507 as in progress with worker 4
[899.821] Worker 0: Starting task assignment
[899.823] Worker 0: Task sent successfully
[899.823] Server 0: Context switching to worker 4
[899.824] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[899.826] UMCG syscall result: 0
[899.826] Worker 0: UMCG registration complete with server 0
[899.826] Worker 0: Entering task processing loop
[899.828] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[899.833] !!!! Initial task 1: STARTING task !!!!
[899.833] !!!! Initial task 1: ABOUT TO SLEEP !!!!
[899.834] UMCG syscall result: 0
[899.834] Server 0: Processing event 1 for worker 4
[899.834] Server 0: Worker 4 blocked
[899.836] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(5d61fd6a-29dd-4b6b-addf-121ccead0507))
[899.837] Server 0: Successfully switched to worker 4
[899.838] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[899.838] Server 0: Attempting to schedule tasks...
[899.839] Server 0: Looking for waiting workers among 3 workers
[899.839] Server 0: Found waiting worker 1 with status Waiting
[899.839] TaskQueue: Attempting to get next task
[899.840] TaskQueue: Retrieved task 229ed386-39b4-4da2-8dea-6010b4a65137
[899.840] Server 0: Assigning task to worker 1
[899.840] WorkerPool: Updating worker 1 status from Waiting to Running
[899.841] TaskQueue: Marking task 229ed386-39b4-4da2-8dea-6010b4a65137 as in progress with worker 5
[899.841] Worker 1: Starting task assignment
[899.842] Worker 1: Task sent successfully
[899.842] Server 0: Context switching to worker 5
[899.842] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[899.843] UMCG syscall result: 0
[899.843] Worker 1: UMCG registration complete with server 0
[899.844] Worker 1: Entering task processing loop
[899.844] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[899.844] !!!! Initial task 2: STARTING task !!!!
[899.845] !!!! Initial task 2: ABOUT TO SLEEP !!!!
[899.845] UMCG syscall result: 0
[899.845] Server 0: Processing event 1 for worker 5
[899.846] Server 0: Worker 5 blocked
[899.846] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(229ed386-39b4-4da2-8dea-6010b4a65137))
[899.846] Server 0: Successfully switched to worker 5
[899.846] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[899.847] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[899.847] UMCG syscall result: 0
[899.848] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[899.848] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[899.848] Server 0: Processing event 2 for worker 6
[899.848] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[899.849] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[899.849] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[899.850] Server 0: Worker 6 unblocking
[899.850] WorkerPool: Updating worker 2 status from Blocked to Running
[899.850] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[899.851] Server 0: Context switching to worker 6
[899.851] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[899.851] TaskHandle: Task submitted successfully
[899.852] !!!! Initial task 0: COMPLETED !!!!
[899.852] Completed task 226ee4dc-2227-442a-8c41-878a87017aa6, total completed: 1/7
[899.853] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[899.854] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[899.854] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[899.855] UMCG syscall result: 0
[899.855] Server 0: Processing event 3 for worker 6
[899.855] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[899.855] Server 0: Got explicit WAIT from worker 6
[899.856] Server 0: Worker 6 current status: Running
[899.856] WorkerPool: Updating worker 2 status from Running to Waiting
[899.856] Server 0: Attempting to schedule tasks...
[899.856] Server 0: Looking for waiting workers among 3 workers
[899.857] Server 0: Found waiting worker 2 with status Waiting
[899.857] TaskQueue: Attempting to get next task
[899.857] TaskQueue: Retrieved task 0ae148df-b30c-4d85-9e58-f72a538bf38b
[899.857] Server 0: Assigning task to worker 2
[899.858] WorkerPool: Updating worker 2 status from Waiting to Running
[899.858] TaskQueue: Marking task 0ae148df-b30c-4d85-9e58-f72a538bf38b as in progress with worker 6
[899.858] Worker 2: Starting task assignment
[899.859] Worker 2: Task sent successfully
[899.859] Server 0: Context switching to worker 6
[899.859] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[899.859] UMCG syscall result: 0
[899.860] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[899.860] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[899.860] !!!! Initial task 3: STARTING task !!!!
[899.861] !!!! Initial task 3: ABOUT TO SLEEP !!!!
[899.861] UMCG syscall result: 0
[899.861] Server 0: Processing event 1 for worker 6
[899.861] Server 0: Worker 6 blocked
[899.862] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(0ae148df-b30c-4d85-9e58-f72a538bf38b))
[899.862] Server 0: Successfully switched to worker 6
[899.863] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[899.863] Server 0: Attempting to schedule tasks...
[899.863] Server 0: Looking for waiting workers among 3 workers
[899.864] Server 0: No waiting workers found
[899.864] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[899.864] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.843] UMCG syscall result: 0
[901.844] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[901.845] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[901.848] Server 0: Processing event 2 for worker 4
[901.849] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.850] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[901.851] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.852] Server 0: Worker 4 unblocking
[901.853] WorkerPool: Updating worker 0 status from Blocked to Running
[901.853] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[901.854] Server 0: Context switching to worker 4
[901.854] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[901.855] !!!! Initial task 1: WOKE UP FROM SLEEP !!!!
[901.855] !!!! Initial task 1: PREPARING to spawn child task !!!!
[901.856] TaskHandle: Submitting new task
[901.856] Executor: Submitting new task
[901.858] Registered task 64253afb-c99e-48c4-a213-d9ac62283bd6, total tasks: 8
[901.859] Executor: Adding task 64253afb-c99e-48c4-a213-d9ac62283bd6 to server 0 (has 3 workers)
[901.859] Server 0: Adding new task
[901.860] Creating new TaskEntry with ID: b109f191-bf0d-4c67-8c2f-b56027dcc899
[901.860] TaskQueue: Enqueueing task b109f191-bf0d-4c67-8c2f-b56027dcc899
[901.862] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[901.862] Server 0: Task queued
[901.863] Task 64253afb-c99e-48c4-a213-d9ac62283bd6 assigned to server 0
[901.863] UMCG syscall result: 0
[901.863] Server 0: Processing event 1 for worker 4
[901.864] Server 0: Worker 4 blocked
[901.864] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(5d61fd6a-29dd-4b6b-addf-121ccead0507))
[901.865] Server 0: Attempting to schedule tasks...
[901.865] Server 0: Looking for waiting workers among 3 workers
[901.866] Server 0: No waiting workers found
[901.866] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[901.866] Server 0: Attempting to schedule tasks...
[901.866] Server 0: Looking for waiting workers among 3 workers
[901.867] Server 0: No waiting workers found
[901.867] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[901.868] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.868] UMCG syscall result: 0
[901.868] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[901.869] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[901.869] Server 0: Processing event 2 for worker 5
[901.869] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.870] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[901.870] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.871] Server 0: Worker 5 unblocking
[901.871] WorkerPool: Updating worker 1 status from Blocked to Running
[901.871] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[901.872] Server 0: Context switching to worker 5
[901.872] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[901.873] !!!! Initial task 2: WOKE UP FROM SLEEP !!!!
[901.873] !!!! Initial task 2: PREPARING to spawn child task !!!!
[901.874] TaskHandle: Submitting new task
[901.874] Executor: Submitting new task
[901.875] Registered task e979a04b-c15d-40cb-8d09-138b6cc004d5, total tasks: 9
[901.875] Executor: Adding task e979a04b-c15d-40cb-8d09-138b6cc004d5 to server 0 (has 3 workers)
[901.876] Server 0: Adding new task
[901.876] Creating new TaskEntry with ID: a5f73866-6d47-4da3-9819-9d7ad4e6c509
[901.876] TaskQueue: Enqueueing task a5f73866-6d47-4da3-9819-9d7ad4e6c509
[901.877] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[901.877] Server 0: Task queued
[901.877] Task e979a04b-c15d-40cb-8d09-138b6cc004d5 assigned to server 0
[901.878] UMCG syscall result: 0
[901.878] Server 0: Processing event 1 for worker 5
[901.878] Server 0: Worker 5 blocked
[901.879] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(229ed386-39b4-4da2-8dea-6010b4a65137))
[901.880] Server 0: Attempting to schedule tasks...
[901.880] Server 0: Looking for waiting workers among 3 workers
[901.881] Server 0: No waiting workers found
[901.881] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[901.881] Server 0: Processing event 2 for worker 6
[901.881] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.882] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[901.882] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.883] Server 0: Worker 6 unblocking
[901.883] WorkerPool: Updating worker 2 status from Blocked to Running
[901.884] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[901.884] Server 0: Context switching to worker 6
[901.884] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[901.884] !!!! Initial task 3: WOKE UP FROM SLEEP !!!!
[901.885] !!!! Initial task 3: PREPARING to spawn child task !!!!
[901.885] TaskHandle: Submitting new task
[901.885] Executor: Submitting new task
[901.886] Registered task b9dffe12-55e2-4720-aff7-5179e222ef42, total tasks: 10
[901.886] Executor: Adding task b9dffe12-55e2-4720-aff7-5179e222ef42 to server 0 (has 3 workers)
[901.887] Server 0: Adding new task
[901.887] Creating new TaskEntry with ID: 167dce9f-b867-4d8f-9878-f502c1151c2e
[901.888] TaskQueue: Enqueueing task 167dce9f-b867-4d8f-9878-f502c1151c2e
[901.888] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[901.889] Server 0: Task queued
[901.889] Task b9dffe12-55e2-4720-aff7-5179e222ef42 assigned to server 0
[901.890] UMCG syscall result: 0
[901.890] Server 0: Processing event 1 for worker 6
[901.890] Server 0: Worker 6 blocked
[901.890] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(0ae148df-b30c-4d85-9e58-f72a538bf38b))
[901.891] Server 0: Attempting to schedule tasks...
[901.891] Server 0: Looking for waiting workers among 3 workers
[901.891] Server 0: No waiting workers found
[901.892] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[901.892] Server 0: Attempting to schedule tasks...
[901.892] Server 0: Looking for waiting workers among 3 workers
[901.893] Server 0: No waiting workers found
[901.893] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[901.893] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.893] UMCG syscall result: 0
[901.894] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[901.894] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[901.894] Server 0: Processing event 2 for worker 4
[901.894] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.895] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[901.895] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.896] Server 0: Worker 4 unblocking
[901.896] WorkerPool: Updating worker 0 status from Blocked to Running
[901.896] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[901.896] Server 0: Context switching to worker 4
[901.897] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[901.897] TaskHandle: Task submitted successfully
[901.898] !!!! Initial task 1: COMPLETED !!!!
[901.898] Completed task f186f0f6-85cd-44f8-8f9b-26fe4d76180d, total completed: 2/10
[901.899] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[901.899] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[901.899] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.901] UMCG syscall result: 0
[901.902] Server 0: Processing event 3 for worker 4
[901.902] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[901.902] Server 0: Got explicit WAIT from worker 4
[901.902] Server 0: Worker 4 current status: Running
[901.903] WorkerPool: Updating worker 0 status from Running to Waiting
[901.903] Server 0: Attempting to schedule tasks...
[901.903] Server 0: Looking for waiting workers among 3 workers
[901.904] Server 0: Found waiting worker 0 with status Waiting
[901.904] TaskQueue: Attempting to get next task
[901.904] TaskQueue: Retrieved task 776353e6-d5a4-4de1-b250-c668d737f467
[901.905] Server 0: Assigning task to worker 0
[901.905] WorkerPool: Updating worker 0 status from Waiting to Running
[901.905] TaskQueue: Marking task 776353e6-d5a4-4de1-b250-c668d737f467 as in progress with worker 4
[901.906] Worker 0: Starting task assignment
[901.906] Worker 0: Task sent successfully
[901.906] Server 0: Context switching to worker 4
[901.907] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[901.907] UMCG syscall result: 0
[901.907] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[901.907] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[901.908] !!!! Initial task 4: STARTING task !!!!
[901.908] !!!! Initial task 4: ABOUT TO SLEEP !!!!
[901.909] UMCG syscall result: 0
[901.909] Server 0: Processing event 1 for worker 4
[901.909] Server 0: Worker 4 blocked
[901.909] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(776353e6-d5a4-4de1-b250-c668d737f467))
[901.910] Server 0: Successfully switched to worker 4
[901.910] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[901.911] Server 0: Processing event 2 for worker 5
[901.911] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.911] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[901.911] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.912] Server 0: Worker 5 unblocking
[901.912] WorkerPool: Updating worker 1 status from Blocked to Running
[901.912] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[901.912] Server 0: Context switching to worker 5
[901.913] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[901.913] TaskHandle: Task submitted successfully
[901.913] !!!! Initial task 2: COMPLETED !!!!
[901.914] Completed task 56861260-d2d2-4243-9d9d-cfe6f072a223, total completed: 3/10
[901.914] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[901.914] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[901.915] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.915] UMCG syscall result: 0
[901.915] Server 0: Processing event 3 for worker 5
[901.916] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[901.916] Server 0: Got explicit WAIT from worker 5
[901.916] Server 0: Worker 5 current status: Running
[901.916] WorkerPool: Updating worker 1 status from Running to Waiting
[901.916] Server 0: Attempting to schedule tasks...
[901.917] Server 0: Looking for waiting workers among 3 workers
[901.917] Server 0: Found waiting worker 1 with status Waiting
[901.917] TaskQueue: Attempting to get next task
[901.917] TaskQueue: Retrieved task 9efa1bbd-3780-47e0-90cf-bc76dd3360b8
[901.918] Server 0: Assigning task to worker 1
[901.918] WorkerPool: Updating worker 1 status from Waiting to Running
[901.918] TaskQueue: Marking task 9efa1bbd-3780-47e0-90cf-bc76dd3360b8 as in progress with worker 5
[901.919] Worker 1: Starting task assignment
[901.919] Worker 1: Task sent successfully
[901.919] Server 0: Context switching to worker 5
[901.920] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[901.920] UMCG syscall result: 0
[901.920] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[901.921] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[901.922] !!!! Initial task 5: STARTING task !!!!
[901.922] !!!! Initial task 5: ABOUT TO SLEEP !!!!
[901.922] UMCG syscall result: 0
[901.922] Server 0: Processing event 1 for worker 5
[901.923] Server 0: Worker 5 blocked
[901.923] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(9efa1bbd-3780-47e0-90cf-bc76dd3360b8))
[901.923] Server 0: Successfully switched to worker 5
[901.924] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[901.924] Server 0: Attempting to schedule tasks...
[901.924] Server 0: Looking for waiting workers among 3 workers
[901.925] Server 0: No waiting workers found
[901.925] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[901.925] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.925] UMCG syscall result: 0
[901.925] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[901.926] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[901.926] Server 0: Processing event 2 for worker 6
[901.926] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[901.927] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[901.927] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[901.928] Server 0: Worker 6 unblocking
[901.928] WorkerPool: Updating worker 2 status from Blocked to Running
[901.928] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[901.929] Server 0: Context switching to worker 6
[901.930] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[901.930] TaskHandle: Task submitted successfully
[901.930] !!!! Initial task 3: COMPLETED !!!!
[901.931] Completed task 306864ca-a621-4f92-810a-8189293ea2b6, total completed: 4/10
[901.931] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[901.931] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[901.932] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[901.932] UMCG syscall result: 0
[901.932] Server 0: Processing event 3 for worker 6
[901.932] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[901.933] Server 0: Got explicit WAIT from worker 6
[901.933] Server 0: Worker 6 current status: Running
[901.933] WorkerPool: Updating worker 2 status from Running to Waiting
[901.934] Server 0: Attempting to schedule tasks...
[901.935] Server 0: Looking for waiting workers among 3 workers
[901.935] Server 0: Found waiting worker 2 with status Waiting
[901.935] TaskQueue: Attempting to get next task
[901.935] TaskQueue: Retrieved task dad3666a-0f2d-4f78-afe8-c0653db9319b
[901.936] Server 0: Assigning task to worker 2
[901.936] WorkerPool: Updating worker 2 status from Waiting to Running
[901.937] TaskQueue: Marking task dad3666a-0f2d-4f78-afe8-c0653db9319b as in progress with worker 6
[901.937] Worker 2: Starting task assignment
[901.938] Worker 2: Task sent successfully
[901.938] Server 0: Context switching to worker 6
[901.938] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[901.939] UMCG syscall result: 0
[901.939] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[901.940] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[901.940] !!!! Child task of initial task 0: STARTING work !!!!
[901.940] UMCG syscall result: 0
[901.941] Server 0: Processing event 1 for worker 6
[901.941] Server 0: Worker 6 blocked
[901.941] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(dad3666a-0f2d-4f78-afe8-c0653db9319b))
[901.942] Server 0: Successfully switched to worker 6
[901.942] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[901.944] Server 0: Attempting to schedule tasks...
[901.944] Server 0: Looking for waiting workers among 3 workers
[901.944] Server 0: No waiting workers found
[901.944] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[901.944] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[902.879] Progress: 4/10 tasks completed
[902.945] UMCG syscall result: 0
[902.946] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[902.947] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[902.948] Server 0: Processing event 2 for worker 6
[902.948] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[902.948] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[902.949] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[902.950] Server 0: Worker 6 unblocking
[902.950] WorkerPool: Updating worker 2 status from Blocked to Running
[902.951] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[902.951] Server 0: Context switching to worker 6
[902.952] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[902.953] !!!! Child task of initial task 0: COMPLETED !!!!
[902.954] Completed task db40c964-376a-4a55-9aac-bf41e058d55e, total completed: 5/10
[902.955] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[902.956] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[902.957] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[902.957] UMCG syscall result: 0
[902.958] Server 0: Processing event 3 for worker 6
[902.959] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[902.959] Server 0: Got explicit WAIT from worker 6
[902.960] Server 0: Worker 6 current status: Running
[902.960] WorkerPool: Updating worker 2 status from Running to Waiting
[902.961] Server 0: Attempting to schedule tasks...
[902.962] Server 0: Looking for waiting workers among 3 workers
[902.962] Server 0: Found waiting worker 2 with status Waiting
[902.963] TaskQueue: Attempting to get next task
[902.963] TaskQueue: Retrieved task b109f191-bf0d-4c67-8c2f-b56027dcc899
[902.964] Server 0: Assigning task to worker 2
[902.964] WorkerPool: Updating worker 2 status from Waiting to Running
[902.965] TaskQueue: Marking task b109f191-bf0d-4c67-8c2f-b56027dcc899 as in progress with worker 6
[902.966] Worker 2: Starting task assignment
[902.966] Worker 2: Task sent successfully
[902.967] Server 0: Context switching to worker 6
[902.967] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[902.967] UMCG syscall result: 0
[902.968] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[902.968] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[902.969] !!!! Child task of initial task 1: STARTING work !!!!
[902.970] UMCG syscall result: 0
[902.970] Server 0: Processing event 1 for worker 6
[902.970] Server 0: Worker 6 blocked
[902.971] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(b109f191-bf0d-4c67-8c2f-b56027dcc899))
[902.971] Server 0: Successfully switched to worker 6
[902.972] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[902.973] Server 0: Attempting to schedule tasks...
[902.973] Server 0: Looking for waiting workers among 3 workers
[902.974] Server 0: No waiting workers found
[902.974] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[902.975] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[902.983] Progress: 5/10 tasks completed
[903.087] Progress: 5/10 tasks completed
[903.200] Progress: 5/10 tasks completed
[903.306] Progress: 5/10 tasks completed
[903.418] Progress: 5/10 tasks completed
[903.530] Progress: 5/10 tasks completed
[903.635] Progress: 5/10 tasks completed
[903.748] Progress: 5/10 tasks completed
[903.918] UMCG syscall result: 0
[903.919] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[903.920] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[903.920] Server 0: Processing event 2 for worker 4
[903.921] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[903.922] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[903.922] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[903.923] Server 0: Worker 4 unblocking
[903.925] WorkerPool: Updating worker 0 status from Blocked to Running
[903.926] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[903.928] Server 0: Context switching to worker 4
[903.928] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[903.929] !!!! Initial task 4: WOKE UP FROM SLEEP !!!!
[903.929] !!!! Initial task 4: PREPARING to spawn child task !!!!
[903.931] TaskHandle: Submitting new task
[903.932] Executor: Submitting new task
[903.933] Registered task 4cf0b8b8-23b3-4fa2-9b8d-a4d92e12d179, total tasks: 11
[903.933] Executor: Adding task 4cf0b8b8-23b3-4fa2-9b8d-a4d92e12d179 to server 0 (has 3 workers)
[903.934] Server 0: Adding new task
[903.934] Creating new TaskEntry with ID: 4fcfb7d1-14ec-4ffb-83a2-100ca904ed3d
[903.935] TaskQueue: Enqueueing task 4fcfb7d1-14ec-4ffb-83a2-100ca904ed3d
[903.935] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[903.936] Server 0: Task queued
[903.936] Task 4cf0b8b8-23b3-4fa2-9b8d-a4d92e12d179 assigned to server 0
[903.937] UMCG syscall result: 0
[903.937] Server 0: Processing event 1 for worker 4
[903.938] Server 0: Worker 4 blocked
[903.938] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(776353e6-d5a4-4de1-b250-c668d737f467))
[903.939] Server 0: Attempting to schedule tasks...
[903.940] Server 0: Looking for waiting workers among 3 workers
[903.940] Server 0: No waiting workers found
[903.941] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[903.941] Server 0: Attempting to schedule tasks...
[903.941] Server 0: Looking for waiting workers among 3 workers
[903.942] Server 0: No waiting workers found
[903.942] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[903.942] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.943] UMCG syscall result: 0
[903.944] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[903.945] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[903.946] Server 0: Processing event 2 for worker 5
[903.946] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[903.946] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[903.947] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[903.947] Server 0: Worker 5 unblocking
[903.948] WorkerPool: Updating worker 1 status from Blocked to Running
[903.948] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[903.949] Server 0: Context switching to worker 5
[903.949] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[903.950] !!!! Initial task 5: WOKE UP FROM SLEEP !!!!
[903.950] !!!! Initial task 5: PREPARING to spawn child task !!!!
[903.951] TaskHandle: Submitting new task
[903.951] Executor: Submitting new task
[903.951] Registered task c8477a8d-cad2-447e-82ce-407a5c95a8a8, total tasks: 12
[903.952] Executor: Adding task c8477a8d-cad2-447e-82ce-407a5c95a8a8 to server 0 (has 3 workers)
[903.952] Server 0: Adding new task
[903.952] Creating new TaskEntry with ID: 4e4498e7-1dd6-46a5-8586-3a28d45b9a19
[903.952] TaskQueue: Enqueueing task 4e4498e7-1dd6-46a5-8586-3a28d45b9a19
[903.953] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[903.954] Server 0: Task queued
[903.954] Task c8477a8d-cad2-447e-82ce-407a5c95a8a8 assigned to server 0
[903.955] UMCG syscall result: 0
[903.955] Server 0: Processing event 1 for worker 5
[903.955] Server 0: Worker 5 blocked
[903.955] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(9efa1bbd-3780-47e0-90cf-bc76dd3360b8))
[903.956] Server 0: Attempting to schedule tasks...
[903.956] Server 0: Looking for waiting workers among 3 workers
[903.959] Server 0: No waiting workers found
[903.959] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[903.959] Server 0: Attempting to schedule tasks...
[903.960] Server 0: Looking for waiting workers among 3 workers
[903.961] Server 0: No waiting workers found
[903.961] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[903.961] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.961] UMCG syscall result: 0
[903.962] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[903.962] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[903.963] Server 0: Processing event 2 for worker 4
[903.963] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[903.963] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[903.964] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[903.964] Server 0: Worker 4 unblocking
[903.965] WorkerPool: Updating worker 0 status from Blocked to Running
[903.965] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[903.966] Server 0: Context switching to worker 4
[903.966] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[903.966] TaskHandle: Task submitted successfully
[903.967] !!!! Initial task 4: COMPLETED !!!!
[903.967] Completed task 7cf19dca-dbf0-492f-b6cc-c88c54dc5312, total completed: 6/12
[903.967] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[903.968] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[903.968] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.969] UMCG syscall result: 0
[903.969] Server 0: Processing event 3 for worker 4
[903.969] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[903.970] Server 0: Got explicit WAIT from worker 4
[903.970] Server 0: Worker 4 current status: Running
[903.971] WorkerPool: Updating worker 0 status from Running to Waiting
[903.971] Server 0: Attempting to schedule tasks...
[903.972] Server 0: Looking for waiting workers among 3 workers
[903.972] Server 0: Found waiting worker 0 with status Waiting
[903.973] TaskQueue: Attempting to get next task
[903.973] TaskQueue: Retrieved task a5f73866-6d47-4da3-9819-9d7ad4e6c509
[903.974] Server 0: Assigning task to worker 0
[903.974] WorkerPool: Updating worker 0 status from Waiting to Running
[903.974] TaskQueue: Marking task a5f73866-6d47-4da3-9819-9d7ad4e6c509 as in progress with worker 4
[903.974] Worker 0: Starting task assignment
[903.975] Worker 0: Task sent successfully
[903.975] Server 0: Context switching to worker 4
[903.975] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[903.975] UMCG syscall result: 0
[903.975] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[903.976] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[903.976] !!!! Child task of initial task 2: STARTING work !!!!
[903.976] UMCG syscall result: 0
[903.976] Server 0: Processing event 1 for worker 4
[903.977] Server 0: Worker 4 blocked
[903.977] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(a5f73866-6d47-4da3-9819-9d7ad4e6c509))
[903.977] Server 0: Successfully switched to worker 4
[903.978] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[903.978] Server 0: Attempting to schedule tasks...
[903.978] Server 0: Looking for waiting workers among 3 workers
[903.978] Server 0: No waiting workers found
[903.978] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[903.979] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.979] UMCG syscall result: 0
[903.979] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[903.979] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[903.979] Server 0: Processing event 2 for worker 5
[903.980] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[903.980] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[903.980] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[903.981] Server 0: Worker 5 unblocking
[903.981] WorkerPool: Updating worker 1 status from Blocked to Running
[903.981] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[903.981] Server 0: Context switching to worker 5
[903.981] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[903.982] TaskHandle: Task submitted successfully
[903.982] !!!! Initial task 5: COMPLETED !!!!
[903.982] Completed task 0b00119c-3679-4305-8456-28aac8bc359d, total completed: 7/12
[903.982] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[903.983] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[903.983] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.983] UMCG syscall result: 0
[903.983] Server 0: Processing event 3 for worker 5
[903.984] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[903.984] Server 0: Got explicit WAIT from worker 5
[903.984] Server 0: Worker 5 current status: Running
[903.984] WorkerPool: Updating worker 1 status from Running to Waiting
[903.985] Server 0: Attempting to schedule tasks...
[903.985] Server 0: Looking for waiting workers among 3 workers
[903.985] Server 0: Found waiting worker 1 with status Waiting
[903.985] TaskQueue: Attempting to get next task
[903.985] TaskQueue: Retrieved task 167dce9f-b867-4d8f-9878-f502c1151c2e
[903.986] Server 0: Assigning task to worker 1
[903.986] WorkerPool: Updating worker 1 status from Waiting to Running
[903.986] TaskQueue: Marking task 167dce9f-b867-4d8f-9878-f502c1151c2e as in progress with worker 5
[903.987] Worker 1: Starting task assignment
[903.987] Worker 1: Task sent successfully
[903.988] Server 0: Context switching to worker 5
[903.988] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[903.988] UMCG syscall result: 0
[903.988] !!!!!!!!!! WORKER 1 [5]: Wait syscall returned 0 !!!!!!!!!!!
[903.989] !!!!!!!!!! WORKER 1: Starting task execution !!!!!!!!!!!
[903.989] !!!! Child task of initial task 3: STARTING work !!!!
[903.989] UMCG syscall result: 0
[903.989] Server 0: Processing event 1 for worker 5
[903.990] Server 0: Worker 5 blocked
[903.990] WorkerPool: Updating worker 1 status from Running to Blocked (keeping task Some(167dce9f-b867-4d8f-9878-f502c1151c2e))
[903.991] Server 0: Successfully switched to worker 5
[903.991] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[903.991] Server 0: Processing event 2 for worker 6
[903.991] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[903.992] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[903.992] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[903.992] Server 0: Worker 6 unblocking
[903.993] WorkerPool: Updating worker 2 status from Blocked to Running
[903.993] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[903.993] Server 0: Context switching to worker 6
[903.994] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[903.994] !!!! Child task of initial task 1: COMPLETED !!!!
[903.995] Completed task 64253afb-c99e-48c4-a213-d9ac62283bd6, total completed: 8/12
[903.995] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[903.995] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[903.995] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[903.996] UMCG syscall result: 0
[903.996] Server 0: Processing event 3 for worker 6
[903.996] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[903.996] Server 0: Got explicit WAIT from worker 6
[903.997] Server 0: Worker 6 current status: Running
[903.997] WorkerPool: Updating worker 2 status from Running to Waiting
[903.997] Server 0: Attempting to schedule tasks...
[903.998] Server 0: Looking for waiting workers among 3 workers
[903.998] Server 0: Found waiting worker 2 with status Waiting
[903.998] TaskQueue: Attempting to get next task
[903.999] TaskQueue: Retrieved task 4fcfb7d1-14ec-4ffb-83a2-100ca904ed3d
[903.999] Server 0: Assigning task to worker 2
[903.999] WorkerPool: Updating worker 2 status from Waiting to Running
[903.999] TaskQueue: Marking task 4fcfb7d1-14ec-4ffb-83a2-100ca904ed3d as in progress with worker 6
[904.000] Worker 2: Starting task assignment
[904.000] Worker 2: Task sent successfully
[904.001] Server 0: Context switching to worker 6
[904.001] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[904.001] UMCG syscall result: 0
[904.001] !!!!!!!!!! WORKER 2 [6]: Wait syscall returned 0 !!!!!!!!!!!
[904.002] !!!!!!!!!! WORKER 2: Starting task execution !!!!!!!!!!!
[904.002] !!!! Child task of initial task 4: STARTING work !!!!
[904.003] UMCG syscall result: 0
[904.003] Server 0: Processing event 1 for worker 6
[904.003] Server 0: Worker 6 blocked
[904.003] WorkerPool: Updating worker 2 status from Running to Blocked (keeping task Some(4fcfb7d1-14ec-4ffb-83a2-100ca904ed3d))
[904.004] Server 0: Successfully switched to worker 6
[904.004] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[904.004] Server 0: Attempting to schedule tasks...
[904.004] Server 0: Looking for waiting workers among 3 workers
[904.004] Server 0: No waiting workers found
[904.005] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[904.005] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[904.986] UMCG syscall result: 0
[904.988] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[904.989] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[904.990] Server 0: Processing event 2 for worker 4
[904.991] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[904.991] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[904.992] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[904.993] Server 0: Worker 4 unblocking
[904.993] WorkerPool: Updating worker 0 status from Blocked to Running
[904.994] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[904.995] Server 0: Context switching to worker 4
[904.995] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[904.996] !!!! Child task of initial task 2: COMPLETED !!!!
[904.997] Completed task e979a04b-c15d-40cb-8d09-138b6cc004d5, total completed: 9/12
[904.998] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[905.000] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[905.000] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[905.002] UMCG syscall result: 0
[905.002] Server 0: Processing event 3 for worker 4
[905.003] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[905.003] Server 0: Got explicit WAIT from worker 4
[905.004] Server 0: Worker 4 current status: Running
[905.005] WorkerPool: Updating worker 0 status from Running to Waiting
[905.006] Server 0: Attempting to schedule tasks...
[905.007] Server 0: Looking for waiting workers among 3 workers
[905.008] Server 0: Found waiting worker 0 with status Waiting
[905.008] TaskQueue: Attempting to get next task
[905.009] TaskQueue: Retrieved task 4e4498e7-1dd6-46a5-8586-3a28d45b9a19
[905.010] Server 0: Assigning task to worker 0
[905.010] WorkerPool: Updating worker 0 status from Waiting to Running
[905.011] TaskQueue: Marking task 4e4498e7-1dd6-46a5-8586-3a28d45b9a19 as in progress with worker 4
[905.012] Worker 0: Starting task assignment
[905.012] Worker 0: Task sent successfully
[905.013] Server 0: Context switching to worker 4
[905.014] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[905.014] UMCG syscall result: 0
[905.014] !!!!!!!!!! WORKER 0 [4]: Wait syscall returned 0 !!!!!!!!!!!
[905.015] !!!!!!!!!! WORKER 0: Starting task execution !!!!!!!!!!!
[905.015] !!!! Child task of initial task 5: STARTING work !!!!
[905.016] UMCG syscall result: 0
[905.016] Server 0: Processing event 1 for worker 4
[905.017] Server 0: Worker 4 blocked
[905.017] WorkerPool: Updating worker 0 status from Running to Blocked (keeping task Some(4e4498e7-1dd6-46a5-8586-3a28d45b9a19))
[905.018] Server 0: Successfully switched to worker 4
[905.018] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[905.021] Server 0: Attempting to schedule tasks...
[905.021] Server 0: Looking for waiting workers among 3 workers
[905.022] Server 0: No waiting workers found
[905.022] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[905.023] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[905.023] UMCG syscall result: 0
[905.023] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[905.024] !!!!!!!!!! SERVER PROCESSING EVENT: 162 !!!!!!!!!!
[905.024] Server 0: Processing event 2 for worker 5
[905.025] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[905.025] !!!!!!!!!! WAKE: Worker 5 current status: Blocked, has task: true !!!!!!!!!!
[905.025] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[905.026] Server 0: Worker 5 unblocking
[905.026] WorkerPool: Updating worker 1 status from Blocked to Running
[905.027] Server 0: switching back to worker after sleep/io WAKE. Worker 5
[905.027] Server 0: Context switching to worker 5
[905.028] UMCG syscall - cmd: CtxSwitch, tid: 5, flags: 0
[905.028] !!!! Child task of initial task 3: COMPLETED !!!!
[905.028] Completed task b9dffe12-55e2-4720-aff7-5179e222ef42, total completed: 10/12
[905.029] !!!!!!!!!! WORKER 1: COMPLETED task execution !!!!!!!!!!!
[905.029] !!!!!!!!!! WORKER 1 [5]: Signaling ready for more work !!!!!!!!!!!
[905.030] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[905.030] UMCG syscall result: 0
[905.030] Server 0: Processing event 3 for worker 5
[905.031] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[905.031] Server 0: Got explicit WAIT from worker 5
[905.031] Server 0: Worker 5 current status: Running
[905.032] WorkerPool: Updating worker 1 status from Running to Waiting
[905.032] Server 0: Attempting to schedule tasks...
[905.032] Server 0: Looking for waiting workers among 3 workers
[905.033] Server 0: Found waiting worker 1 with status Waiting
[905.033] TaskQueue: Attempting to get next task
[905.033] TaskQueue: No tasks available
[905.034] !!!!!!!!!! SERVER PROCESSING EVENT: 194 !!!!!!!!!!
[905.034] Server 0: Processing event 2 for worker 6
[905.034] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[905.034] !!!!!!!!!! WAKE: Worker 6 current status: Blocked, has task: true !!!!!!!!!!
[905.035] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[905.036] Server 0: Worker 6 unblocking
[905.036] WorkerPool: Updating worker 2 status from Blocked to Running
[905.036] Server 0: switching back to worker after sleep/io WAKE. Worker 6
[905.036] Server 0: Context switching to worker 6
[905.037] UMCG syscall - cmd: CtxSwitch, tid: 6, flags: 0
[905.037] !!!! Child task of initial task 4: COMPLETED !!!!
[905.037] Completed task 4cf0b8b8-23b3-4fa2-9b8d-a4d92e12d179, total completed: 11/12
[905.038] !!!!!!!!!! WORKER 2: COMPLETED task execution !!!!!!!!!!!
[905.038] !!!!!!!!!! WORKER 2 [6]: Signaling ready for more work !!!!!!!!!!!
[905.038] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[905.039] UMCG syscall result: 0
[905.039] Server 0: Processing event 3 for worker 6
[905.039] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[905.039] Server 0: Got explicit WAIT from worker 6
[905.040] Server 0: Worker 6 current status: Running
[905.040] WorkerPool: Updating worker 2 status from Running to Waiting
[905.041] Server 0: Attempting to schedule tasks...
[905.041] Server 0: Looking for waiting workers among 3 workers
[905.041] Server 0: Found waiting worker 2 with status Waiting
[905.041] TaskQueue: Attempting to get next task
[905.042] TaskQueue: No tasks available
[905.042] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[905.042] Server 0: Attempting to schedule tasks...
[905.042] Server 0: Looking for waiting workers among 3 workers
[905.043] Server 0: Found waiting worker 2 with status Waiting
[905.043] TaskQueue: Attempting to get next task
[905.043] TaskQueue: No tasks available
[905.044] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[905.044] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[906.029] UMCG syscall result: 0
[906.030] !!!!!!!!!! SERVER EVENT WAIT RETURNED: 0 !!!!!!!!!!
[906.031] !!!!!!!!!! SERVER PROCESSING EVENT: 130 !!!!!!!!!!
[906.031] Server 0: Processing event 2 for worker 4
[906.032] !!!!!!!!!! WAKE EVENT START - CHECKING WORKER STATE !!!!!!!!!!
[906.032] !!!!!!!!!! WAKE: Worker 4 current status: Blocked, has task: true !!!!!!!!!!
[906.033] !!!!!!!!!! WAKE: This is a sleep/IO wakeup WAKE (worker was Blocked) !!!!!!!!!!
[906.033] Server 0: Worker 4 unblocking
[906.034] WorkerPool: Updating worker 0 status from Blocked to Running
[906.034] Server 0: switching back to worker after sleep/io WAKE. Worker 4
[906.035] Server 0: Context switching to worker 4
[906.035] UMCG syscall - cmd: CtxSwitch, tid: 4, flags: 0
[906.036] !!!! Child task of initial task 5: COMPLETED !!!!
[906.036] Completed task c8477a8d-cad2-447e-82ce-407a5c95a8a8, total completed: 12/12
[906.036] !!!!!!!!!! WORKER 0: COMPLETED task execution !!!!!!!!!!!
[906.037] !!!!!!!!!! WORKER 0 [4]: Signaling ready for more work !!!!!!!!!!!
[906.038] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[906.038] UMCG syscall result: 0
[906.038] Server 0: Processing event 3 for worker 4
[906.039] !!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!
[906.039] Server 0: Got explicit WAIT from worker 4
[906.040] Server 0: Worker 4 current status: Running
[906.040] WorkerPool: Updating worker 0 status from Running to Waiting
[906.041] Server 0: Attempting to schedule tasks...
[906.041] Server 0: Looking for waiting workers among 3 workers
[906.042] Server 0: Found waiting worker 2 with status Waiting
[906.042] TaskQueue: Attempting to get next task
[906.043] TaskQueue: No tasks available
[906.043] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[906.043] Server 0: Attempting to schedule tasks...
[906.044] Server 0: Looking for waiting workers among 3 workers
[906.045] Server 0: Found waiting worker 2 with status Waiting
[906.045] TaskQueue: Attempting to get next task
[906.046] TaskQueue: No tasks available
[906.046] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[906.046] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[906.070] All tasks completed successfully (12/12)
[906.071] Initiating executor shutdown...
[906.071] Server 0: Adding new task
[906.072] Server 0: Received shutdown signal
Running run_multi_server_demo
➜  /app git:(master) ✗


// Failure Case
➜  /app git:(master) ✗ ops run -c config.json target/x86_64-unknown-linux-musl/release/UMCG
running local instance
booting /root/.ops/images/UMCG ...
en1: assigned 10.0.2.15
Running full test suite...
what the fuck
Running run_dynamic_task_demo
[805.545] Creating new Executor
[805.559] Creating Server 0 on CPU 0
[805.567] Creating Server 0
[805.571] Creating new TaskQueue
[805.577] Creating WorkerPool with capacity for 3 workers
[805.581] Starting executor...
[805.581] Executor: Starting servers
[805.605] Starting server 0 initialization
[805.607] Server 0 ready for tasks
[805.620] Successfully set CPU affinity to 0
[805.622] Server 0: Starting up
[805.627] All 1 servers started and ready
[805.629] UMCG syscall - cmd: RegisterServer, tid: 0, flags: 0
[805.636] UMCG syscall result: 0
[805.637] Server 0: UMCG registration complete
[805.638] Server 0: Initializing workers
[805.639] Server 0: Initializing worker 0
[805.642] Starting WorkerThread 0 for server 0
[805.652] Successfully set CPU affinity to 0
[805.654] Worker 0: Initialized with tid 4
[805.656] Worker 0: Registering with UMCG (worker_id: 128) for server 0
[805.657] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[805.659] WorkerThread 0 started with tid 4 for server 0
[805.662] Creating new Worker 0 for server 0
[805.662] WorkerPool: Adding worker 0 with tid 4
[805.665] WorkerPool: Updating worker 0 status from Initializing to Registering
[805.666] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[805.667] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[805.669] UMCG syscall result: 0
[805.669] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[805.670] Worker 0: Registering worker event 130
[805.671] WorkerPool: Updating worker 0 status from Registering to Waiting
[805.671] Server 0: Worker 0 initialized successfully
[805.672] Server 0: Initializing worker 1
[805.674] Starting WorkerThread 1 for server 0
[805.679] Successfully set CPU affinity to 0
[805.679] Worker 1: Initialized with tid 5
[805.682] Worker 1: Registering with UMCG (worker_id: 160) for server 0
[805.683] WorkerThread 1 started with tid 5 for server 0
[805.684] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[805.684] Creating new Worker 1 for server 0
[805.685] WorkerPool: Adding worker 1 with tid 5
[805.686] WorkerPool: Updating worker 1 status from Initializing to Registering
[805.686] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[805.687] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[805.688] UMCG syscall result: 0
[805.688] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[805.688] Worker 0: Registering worker event 162
[805.688] WorkerPool: Updating worker 1 status from Registering to Waiting
[805.694] Server 0: Worker 1 initialized successfully
[805.694] Submitting initial tasks...
[805.695] Server 0: Initializing worker 2
[805.695] !!!! Submitting initial task 0 !!!!
[805.696] Starting WorkerThread 2 for server 0
[805.699] Executor: Submitting new task
[805.703] Successfully set CPU affinity to 0
[805.703] Worker 2: Initialized with tid 6
[805.705] Worker 2: Registering with UMCG (worker_id: 192) for server 0
[805.714] WorkerThread 2 started with tid 6 for server 0
[805.715] UMCG syscall - cmd: RegisterWorker, tid: 0, flags: 0
[805.715] Creating new Worker 2 for server 0
[805.715] WorkerPool: Adding worker 2 with tid 6
[805.716] WorkerPool: Updating worker 2 status from Initializing to Registering
[805.716] !!!!!!!!!! UMCG WAIT RETRY START - worker: 0, flags: 0 !!!!!!!!!!
[805.716] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[805.717] UMCG syscall result: 0
[805.717] !!!!!!!!!! UMCG WAIT RETRY RETURNED: 0 !!!!!!!!!!
[805.721] Worker 0: Registering worker event 194
[805.719] Registered task cad46540-a183-469b-a262-3a6551736c2f, total tasks: 1
[805.722] WorkerPool: Updating worker 2 status from Registering to Waiting
[805.728] Server 0: Worker 2 initialized successfully
[805.729] Server 0: All workers initialized
[805.730] !!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!
[805.730] Executor: Adding task cad46540-a183-469b-a262-3a6551736c2f to server 0 (has 3 workers)
[805.730] Server 0: Attempting to schedule tasks...
[805.731] Server 0: Adding new task
[805.731] Server 0: Looking for waiting workers among 3 workers
[805.732] Server 0: Found waiting worker 1 with status Waiting
[805.733] Creating new TaskEntry with ID: 972febdf-3558-4f80-8952-e3182196f81c
[805.734] TaskQueue: Attempting to get next task
[805.735] TaskQueue: No tasks available
[805.736] TaskQueue: Enqueueing task 972febdf-3558-4f80-8952-e3182196f81c
[805.737] !!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!
[805.737] UMCG syscall - cmd: Wait, tid: 0, flags: 0
[805.739] TaskQueue stats - Pending: 1, Preempted: 0, In Progress: 0
[805.740] Server 0: Task queued
[805.742] Task cad46540-a183-469b-a262-3a6551736c2f assigned to server 0
[805.743] !!!! Submitting initial task 1 !!!!
[805.743] Executor: Submitting new task
[805.746] Registered task 49373838-d5db-4189-8326-d411966db874, total tasks: 2
[805.746] Executor: Adding task 49373838-d5db-4189-8326-d411966db874 to server 0 (has 3 workers)
[805.747] Server 0: Adding new task
[805.747] Creating new TaskEntry with ID: ccffd24c-9ab3-4339-82b0-5f44e8463414
[805.748] TaskQueue: Enqueueing task ccffd24c-9ab3-4339-82b0-5f44e8463414
[805.749] TaskQueue stats - Pending: 2, Preempted: 0, In Progress: 0
[805.749] Server 0: Task queued
[805.750] Task 49373838-d5db-4189-8326-d411966db874 assigned to server 0
[805.750] !!!! Submitting initial task 2 !!!!
[805.750] Executor: Submitting new task
[805.751] Registered task f536eef9-3f4a-4f8e-8178-c43cdf44d1b5, total tasks: 3
[805.751] Executor: Adding task f536eef9-3f4a-4f8e-8178-c43cdf44d1b5 to server 0 (has 3 workers)
[805.752] Server 0: Adding new task
[805.752] Creating new TaskEntry with ID: 17dc398c-aa39-43bd-a657-aa1a703b6d7d
[805.753] TaskQueue: Enqueueing task 17dc398c-aa39-43bd-a657-aa1a703b6d7d
[805.753] TaskQueue stats - Pending: 3, Preempted: 0, In Progress: 0
[805.754] Server 0: Task queued
[805.754] Task f536eef9-3f4a-4f8e-8178-c43cdf44d1b5 assigned to server 0
[805.754] !!!! Submitting initial task 3 !!!!
[805.754] Executor: Submitting new task
[805.756] Registered task 9d9a8eaa-34cd-4f6f-ae2f-79940119e53a, total tasks: 4
[805.756] Executor: Adding task 9d9a8eaa-34cd-4f6f-ae2f-79940119e53a to server 0 (has 3 workers)
[805.757] Server 0: Adding new task
[805.757] Creating new TaskEntry with ID: 9135d0a2-dc8b-4d22-837b-fc429aafa68a
[805.758] TaskQueue: Enqueueing task 9135d0a2-dc8b-4d22-837b-fc429aafa68a
[805.758] TaskQueue stats - Pending: 4, Preempted: 0, In Progress: 0
[805.758] Server 0: Task queued
[805.758] Task 9d9a8eaa-34cd-4f6f-ae2f-79940119e53a assigned to server 0
[805.758] !!!! Submitting initial task 4 !!!!
[805.759] Executor: Submitting new task
[805.759] Registered task d72c1e55-11c4-4aed-96f4-1e67a95c2d09, total tasks: 5
[805.760] Executor: Adding task d72c1e55-11c4-4aed-96f4-1e67a95c2d09 to server 0 (has 3 workers)
[805.760] Server 0: Adding new task
[805.760] Creating new TaskEntry with ID: 90ec0176-1874-4a51-a4f1-4f182440d407
[805.761] TaskQueue: Enqueueing task 90ec0176-1874-4a51-a4f1-4f182440d407
[805.761] TaskQueue stats - Pending: 5, Preempted: 0, In Progress: 0
[805.761] Server 0: Task queued
[805.761] Task d72c1e55-11c4-4aed-96f4-1e67a95c2d09 assigned to server 0
[805.762] !!!! Submitting initial task 5 !!!!
[805.762] Executor: Submitting new task
[805.762] Registered task 7d3759f8-d3cf-44d5-977c-96f04af47218, total tasks: 6
[805.762] Executor: Adding task 7d3759f8-d3cf-44d5-977c-96f04af47218 to server 0 (has 3 workers)
[805.763] Server 0: Adding new task
[805.763] Creating new TaskEntry with ID: 06ffc9b5-a32a-41d0-96b2-e0fc8ef95f19
[805.764] TaskQueue: Enqueueing task 06ffc9b5-a32a-41d0-96b2-e0fc8ef95f19
[805.764] TaskQueue stats - Pending: 6, Preempted: 0, In Progress: 0
[805.764] Server 0: Task queued
[805.764] Task 7d3759f8-d3cf-44d5-977c-96f04af47218 assigned to server 0
[805.765] All tasks submitted, waiting for completion...
[805.767] Progress: 0/6 tasks completed
[805.876] Progress: 0/6 tasks completed
[805.986] Progress: 0/6 tasks completed
[806.091] Progress: 0/6 tasks completed
[806.203] Progress: 0/6 tasks completed
[806.313] Progress: 0/6 tasks completed
[806.421] Progress: 0/6 tasks completed
[806.527] Progress: 0/6 tasks completed
[806.630] Progress: 0/6 tasks completed
[806.735] Progress: 0/6 tasks completed
en1: assigned FE80::C1E:77FF:FE95:2DD2
[810.836] Progress: 0/6 tasks completed
[810.940] Progress: 0/6 tasks completed
[811.046] Progress: 0/6 tasks completed
[811.153] Progress: 0/6 tasks completed
[811.263] Progress: 0/6 tasks completed
[811.372] Progress: 0/6 tasks completed
[811.478] Progress: 0/6 tasks completed
[811.589] Progress: 0/6 tasks completed
[811.700] Progress: 0/6 tasks completed
[815.796] Progress: 0/6 tasks completed
[815.903] Progress: 0/6 tasks completed
[816.014] Progress: 0/6 tasks completed
[816.121] Progress: 0/6 tasks completed
[816.228] Progress: 0/6 tasks completed
[816.331] Progress: 0/6 tasks completed
[816.437] Progress: 0/6 tasks completed
[816.542] Progress: 0/6 tasks completed
[816.650] Progress: 0/6 tasks completed
[816.755] Progress: 0/6 tasks completed
[820.855] Progress: 0/6 tasks completed
[820.959] Progress: 0/6 tasks completed
[821.064] Progress: 0/6 tasks completed
[821.168] Progress: 0/6 tasks completed
[821.277] Progress: 0/6 tasks completed
[821.387] Progress: 0/6 tasks completed
[821.498] Progress: 0/6 tasks completed
[821.604] Progress: 0/6 tasks completed
[821.716] Progress: 0/6 tasks completed
[825.813] Progress: 0/6 tasks completed
[825.919] Progress: 0/6 tasks completed
[826.026] Progress: 0/6 tasks completed
[826.129] Progress: 0/6 tasks completed
[826.232] Progress: 0/6 tasks completed
[826.335] Progress: 0/6 tasks completed
[826.439] Progress: 0/6 tasks completed
[826.545] Progress: 0/6 tasks completed
[826.649] Progress: 0/6 tasks completed
[826.758] Progress: 0/6 tasks completed
[830.775] Progress: 0/6 tasks completed
[830.881] Progress: 0/6 tasks completed
[830.987] Progress: 0/6 tasks completed
[831.094] Progress: 0/6 tasks completed
[831.205] Progress: 0/6 tasks completed
[831.311] Progress: 0/6 tasks completed
[831.417] Progress: 0/6 tasks completed
[831.523] Progress: 0/6 tasks completed
[831.631] Progress: 0/6 tasks completed
[831.735] Progress: 0/6 tasks completed
[835.833] Timeout waiting for tasks to complete! (0/6 completed)
[835.834] Initiating executor shutdown...
[835.836] Server 0: Adding new task
[835.837] Server 0: Received shutdown signal




*/


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