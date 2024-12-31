use std::collections::{HashMap, HashSet};
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use libc::pid_t;
use uuid::Uuid;
use std::thread::{self, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use log::error;
use crate::{umcg_base, ServerError};
use crate::umcg_base::{UmcgCmd, UmcgEventType, DEBUG_LOGGING, EVENT_BUFFER_SIZE, SYS_UMCG_CTL, UMCG_WAIT_FLAG_INTERRUPTED, UMCG_WORKER_EVENT_MASK, UMCG_WORKER_ID_SHIFT, WORKER_REGISTRATION_TIMEOUT_MS};

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

type Task = Box<dyn FnOnce() + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
enum WorkerStatus {
    Initializing,    // Worker thread started but not registered with UMCG
    Registering,     // UMCG registration in progress
    Running,         // Actively executing a task
    Blocked,         // Blocked on I/O or syscall
    Waiting,         // Ready for new tasks
    Completed,       // Worker has finished and unregistered
}

enum WorkerTask {
    Function(Task),
    Shutdown,
}

#[derive(Clone)]
struct ExecutorConfig {
    worker_count: usize,
    server_count: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            worker_count: 3,
            server_count: 1,
        }
    }
}

struct Worker {
    id: usize,
    task_rx: Receiver<WorkerTask>,
}

impl Worker {
    fn new(id: usize, task_rx: Receiver<WorkerTask>) -> Self {
        Self {
            id,
            task_rx,
        }
    }

    fn spawn(
        id: usize,
        states: Arc<Mutex<HashMap<u64, Mutex<WorkerStatus>>>>,
        channels: Arc<Mutex<HashMap<u64, Sender<WorkerTask>>>>
    ) -> JoinHandle<()> {
        let (tx, rx) = channel();
        let worker = Worker::new(id, rx);

        let handle = thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };
            let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;  // Shift the tid for storage

            // Add worker state and channel to the shared maps
            {
                let mut states = states.lock().unwrap();
                states.insert(worker_id, Mutex::new(WorkerStatus::Initializing));

                let mut channels = channels.lock().unwrap();
                channels.insert(worker_id, tx);
            }

            debug!("Worker {} spawned with tid {} (id: {})", worker.id, tid, worker_id);
            worker.start();
        });

        handle
    }

    fn start(&self) {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };
        debug!("Worker {} starting UMCG registration with tid {}", self.id, tid);

        // Register with UMCG
        let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;
        let reg_result = unsafe {
            libc::syscall(
                SYS_UMCG_CTL as i64,
                0,
                UmcgCmd::RegisterWorker as i64,
                0,
                worker_id as i64,
                std::ptr::null_mut::<u64>() as i64,
                0
            )
        };

        if reg_result != 0 {
            error!("Worker {} UMCG registration failed: {}", self.id, reg_result);
            return;
        }

        debug!("Worker {} UMCG registration complete", self.id);

        // Enter event loop
        loop {
            match self.task_rx.try_recv() {
                Ok(WorkerTask::Function(task)) => {
                    debug!("Worker {} executing task", self.id);
                    task();

                    // Signal ready for more work with UMCG wait
                    debug!("Worker {} signaling ready for work", self.id);
                    let wait_result = unsafe {
                        libc::syscall(
                            SYS_UMCG_CTL as i64,
                            0,
                            UmcgCmd::Wait as i64,
                            0,
                            0,
                            std::ptr::null_mut::<u64>() as i64,
                            0
                        )
                    };

                    if wait_result != 0 {
                        error!("Worker {} UMCG wait failed: {}", self.id, wait_result);
                        break;
                    }
                }
                Ok(WorkerTask::Shutdown) => {
                    debug!("Worker {} received shutdown signal", self.id);
                    break;
                }
                Err(TryRecvError::Empty) => {
                    // No tasks available, enter wait state
                    let wait_result = unsafe {
                        libc::syscall(
                            SYS_UMCG_CTL as i64,
                            0,
                            UmcgCmd::Wait as i64,
                            0,
                            0,
                            std::ptr::null_mut::<u64>() as i64,
                            0
                        )
                    };

                    if wait_result != 0 {
                        error!("Worker {} UMCG wait failed: {}", self.id, wait_result);
                        break;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    debug!("Worker {} channel disconnected", self.id);
                    break;
                }
            }
        }

        // Unregister from UMCG
        debug!("Worker {} unregistering from UMCG", self.id);
        let unreg_result = unsafe {
            libc::syscall(
                SYS_UMCG_CTL as i64,
                0,
                UmcgCmd::Unregister as i64,
                0,
                0,
                std::ptr::null_mut::<u64>() as i64,
                0
            )
        };

        if unreg_result != 0 {
            error!("Worker {} UMCG unregistration failed: {}", self.id, unreg_result);
        }

        debug!("Worker {} shutdown complete", self.id);
    }
}

struct WorkerQueues {
    pending: ArrayQueue<u64>,
    running: ArrayQueue<u64>,
    preempted: ArrayQueue<u64>,
}

impl WorkerQueues {
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: ArrayQueue::new(capacity),
            running: ArrayQueue::new(capacity),
            preempted: ArrayQueue::new(capacity)
        }
    }
}

struct Server {
    id: usize,
    states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
    channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
    manage_tasks: Arc<dyn ManageTask>,
    worker_queues: Arc<WorkerQueues>,
    done: Arc<AtomicBool>
}

impl Server {
    pub fn new(
        id: usize,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        manage_tasks: Arc<dyn ManageTask>,
        worker_queues: Arc<WorkerQueues>,
        done: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            states: states.clone(),
            channels: channels.clone(),
            manage_tasks,
            worker_queues: worker_queues.clone(),
            done,
        }
    }

    fn start_initial_server(mut self) -> JoinHandle<()> {
        thread::spawn(move || {
            // Register server with UMCG
            let reg_result = unsafe {
                libc::syscall(
                    SYS_UMCG_CTL as i64,
                    0,
                    UmcgCmd::RegisterServer as i64,
                    0,
                    0,
                    std::ptr::null_mut::<u64>() as i64,
                    0
                )
            };

            if reg_result == 0 {
                debug!("Server {}: UMCG registration complete", self.id);
            } else {
                debug!("Server {}: UMCG registration failed: {}", self.id, reg_result);
                return;
            }

            // Now wait for UMCG worker registration events
            debug!("Server {}: Waiting for worker registration events", self.id);
            let mut events = [0u64; EVENT_BUFFER_SIZE];

            // Keep track of registered workers
            let mut registered_workers = HashSet::new();

            // Process registration for each worker
            while registered_workers.len() < self.states.len() {
                let ret = umcg_base::umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
                if ret != 0 {
                    error!("Server {}: Wait failed during worker registration: {}", self.id, ret);
                    continue;
                }

                let event = events[0];
                let event_type = event & UMCG_WORKER_EVENT_MASK;
                let worker_tid = (event >> UMCG_WORKER_ID_SHIFT) as i32;  // Raw tid for context switch
                let worker_id = (worker_tid as u64) << UMCG_WORKER_ID_SHIFT;  // Shifted ID for our maps

                debug!("Server {}: Got event type {} for worker tid {} (id: {})",
                self.id, event_type, worker_tid, worker_id);

                if event_type == UmcgEventType::Wake as u64 {
                    if registered_workers.contains(&worker_id) {
                        debug!("Server {}: Already registered worker {}, skipping", self.id, worker_id);
                        continue;
                    }

                    debug!("Server {}: Got wake event for worker {}, doing context switch to tid {}",
                    self.id, worker_id, worker_tid);

                    // Context switch to let worker enter wait state
                    let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
                    let switch_ret = unsafe {
                        libc::syscall(
                            SYS_UMCG_CTL as i64,
                            0,
                            UmcgCmd::CtxSwitch as i64,
                            worker_tid,  // Use raw tid here
                            0,
                            switch_events.as_mut_ptr() as i64,
                            EVENT_BUFFER_SIZE as i64
                        )
                    };

                    debug!("Server {}: Context switch returned {} for worker tid {} (id: {})",
                    self.id, switch_ret, worker_tid, worker_id);

                    if switch_ret == 0 {
                        // Update worker status and add to pending queue
                        if let Some(status) = self.states.get(&worker_id) {
                            let mut status = status.lock().unwrap();
                            *status = WorkerStatus::Waiting;
                            if self.worker_queues.pending.push(worker_id).is_err() {
                                error!("Server {}: Failed to add worker {} to pending queue",
                                self.id, worker_id);
                            }
                            debug!("Server {}: Worker {} status set to waiting and added to pending queue",
                            self.id, worker_id);

                            registered_workers.insert(worker_id);
                        } else {
                            error!("Server {}: No state found for worker ID {}", self.id, worker_id);
                        }
                    } else {
                        error!("Server {}: Context switch failed for worker tid {} (id: {}): {}",
                        self.id, worker_tid, worker_id, switch_ret);
                    }
                } else {
                    debug!("Server {}: Unexpected event {} during worker registration",
                    self.id, event_type);
                }
            }

            debug!("Server {}: Worker registration complete, registered {} workers",
            self.id, registered_workers.len());

            // Start the event loop
            self.run_event_loop().expect("REASON")
        })
    }

    fn run_event_loop(&self) -> Result<(), ServerError> {
        debug!("Server {}: Starting event loop", self.id);

        while !self.done.load(Ordering::Relaxed) {
            // Try to schedule any pending tasks first
            debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!");
            self.try_schedule_tasks()?;

            let mut events = [0u64; 6];

            // Calculate absolute timeout (now + 100ms)
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let abs_timeout = now.as_nanos() as u64 + 1_000_000_000;

            debug!("!!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!");
            let ret = umcg_base::sys_umcg_ctl(
                0,
                UmcgCmd::Wait,
                0,
                abs_timeout,  // Use absolute timeout value
                Some(&mut events),
                6
            );

            debug!("!!!!!!!!!! SERVER EVENT WAIT RETURNED: {} !!!!!!!!!!", ret);

            if ret != 0 && unsafe { *libc::__errno_location() } != libc::ETIMEDOUT {
                error!("Server {} wait error: {}", self.id, ret);
                return Err(ServerError::SystemError(std::io::Error::last_os_error()));
            }

            // Process any events we received
            for &event in events.iter().take_while(|&&e| e != 0) {
                debug!("!!!!!!!!!! SERVER PROCESSING EVENT: {} !!!!!!!!!!", event);
                if let Err(e) = self.handle_event(event) {
                    error!("Server {} event handling error: {}", self.id, e);
                }

                debug!("!!!!!!!!!! SERVER EVENT HANDLING COMPLETE - Queue stats - Pending: {}, Running: {}, Preempted: {} !!!!!!!!!!",
                    self.worker_queues.pending.len(),
                    self.worker_queues.running.len(),
                    self.worker_queues.preempted.len());
            }
        }

        debug!("Server {}: Event loop terminated", self.id);
        Ok(())
    }


    fn try_schedule_tasks(&self) -> Result<(), ServerError> {
        debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS TO SCHEDULE !!!!!!!!!!");
        debug!("!!!! TRY_SCHEDULE: Checking pending queue size: {} !!!!",
           self.worker_queues.pending.len());
        debug!("!!!! TRY_SCHEDULE: Checking running queue size: {} !!!!",
           self.worker_queues.running.len());

        // Try to get a pending worker from the queue
        if let Some(worker_id) = self.worker_queues.pending.pop() {
            debug!("Server {}: Found pending worker {}", self.id, worker_id);

            // Verify worker is actually in waiting state
            if let Some(status) = self.states.get(&worker_id) {
                let status = status.lock().unwrap();
                debug!("Server {}: Worker {} status is {:?}", self.id, worker_id, *status);

                if *status == WorkerStatus::Waiting {
                    // Try to get a task
                    debug!("Server {}: Attempting to get task from queue", self.id);
                    match self.manage_tasks.remove_task() {
                        Some(task) => {
                            debug!("!!!! TRY_SCHEDULE: Found task for worker {} !!!!", worker_id);
                            debug!("Server {}: Found task for worker {}", self.id, worker_id);
                            if let Some(tx) = self.channels.get(&worker_id) {
                                // Update status and move to running queue
                                drop(status); // Release the lock before updating
                                if let Some(status) = self.states.get(&worker_id) {
                                    let mut status = status.lock().unwrap();
                                    *status = WorkerStatus::Running;

                                    // Add to running queue
                                    if self.worker_queues.running.push(worker_id).is_err() {
                                        error!("Server {}: Failed to add worker {} to running queue",
                                        self.id, worker_id);
                                    }
                                    debug!("Server {}: Updated worker {} status to Running", self.id, worker_id);
                                }

                                // Send task to worker
                                if tx.send(WorkerTask::Function(task)).is_ok() {
                                    debug!("Server {}: Sent task to worker {}", self.id, worker_id);

                                    // Context switch to the worker to let it start the task
                                    let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
                                    debug!("Server {}: Context switching to worker {} to start task", self.id, worker_id);
                                    let switch_ret = unsafe {
                                        libc::syscall(
                                            SYS_UMCG_CTL as i64,
                                            0,
                                            UmcgCmd::CtxSwitch as i64,
                                            (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,
                                            0,
                                            switch_events.as_mut_ptr() as i64,
                                            EVENT_BUFFER_SIZE as i64
                                        )
                                    };
                                    debug!("Server {}: Context switch returned {} for worker {}",
                                    self.id, switch_ret, worker_id);

                                    // Process any events from the context switch
                                    for &event in switch_events.iter().take_while(|&&e| e != 0) {
                                        debug!("Server {}: Got event {} from context switch", self.id, event);
                                        self.handle_event(event)?;
                                    }
                                }
                            }
                        }
                        None => {
                            debug!("!!!! TRY_SCHEDULE: No tasks available for worker {} !!!!", worker_id);
                            debug!("Server {}: No tasks available, returning worker {} to pending queue",
                            self.id, worker_id);
                            // No tasks available, put worker back in pending queue
                            if self.worker_queues.pending.push(worker_id).is_err() {
                                error!("Server {}: Failed to return worker {} to pending queue",
                                self.id, worker_id);
                            }
                        }
                    }
                } else {
                    debug!("Server {}: Worker {} not in waiting state", self.id, worker_id);
                    // Worker not in waiting state, put back in pending queue
                    if self.worker_queues.pending.push(worker_id).is_err() {
                        error!("Server {}: Failed to return worker {} to pending queue",
                        self.id, worker_id);
                    }
                }
            }
        } else {
            debug!("Server {}: No pending workers available", self.id);
        }
        Ok(())
    }

    fn handle_event(&self, event: u64) -> Result<(), ServerError> {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;

        debug!("Server {}: Processing event type {} from worker {} (raw event: {})",
        self.id, event_type, worker_id, event);

        match event_type {
            e if e == UmcgEventType::Block as u64 => {
                debug!("Server {}: Worker {} blocked while in running queue", self.id, worker_id);

                // Get current status and update it under a short lock
                let current_status = {
                    if let Some(status) = self.states.get(&worker_id) {
                        let mut status = status.lock().unwrap();
                        debug!("Server {}: Processing BLOCK for worker {} in state {:?}",
                        self.id, worker_id, *status);
                        if *status == WorkerStatus::Running {
                            *status = WorkerStatus::Blocked;
                            debug!("Server {}: Updated worker {} status from Running to Blocked",
                            self.id, worker_id);
                        }
                        status.clone()
                    } else {
                        debug!("Server {}: No state found for worker {} during BLOCK",
                        self.id, worker_id);
                        return Ok(());
                    }
                };
                // Worker stays in running queue while blocked
                debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
                event_type, worker_id);
            },

            e if e == UmcgEventType::Wake as u64 => {
                // Get current status under a short lock
                let current_status = {
                    if let Some(status) = self.states.get(&worker_id) {
                        let status = status.lock().unwrap();
                        status.clone()
                    } else {
                        return Ok(());
                    }
                };

                debug!("Server {}: Processing WAKE for worker {} in state {:?}",
                self.id, worker_id, current_status);

                match current_status {
                    WorkerStatus::Blocked => {
                        debug!("Server {}: Worker {} woke up from blocked state, resuming task",
                        self.id, worker_id);

                        // Update status under a short lock
                        if let Some(status) = self.states.get(&worker_id) {
                            let mut status = status.lock().unwrap();
                            *status = WorkerStatus::Running;
                            debug!("Server {}: Updated worker {} status from Blocked to Running",
                            self.id, worker_id);
                        }

                        // Do context switch after releasing lock
                        debug!("Server {}: Context switching to unblocked worker {} to resume task",
                        self.id, worker_id);
                        let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
                        let switch_ret = unsafe {
                            libc::syscall(
                                SYS_UMCG_CTL as i64,
                                0,
                                UmcgCmd::CtxSwitch as i64,
                                (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,
                                0,
                                switch_events.as_mut_ptr() as i64,
                                EVENT_BUFFER_SIZE as i64
                            )
                        };
                        debug!("Server {}: Context switch for unblocked worker {} returned {}",
                        self.id, worker_id, switch_ret);

                        // Process any events from the context switch
                        for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
                            debug!("Server {}: Got event {} from unblock context switch",
                            self.id, switch_event);
                            self.handle_event(switch_event)?;
                        }
                    },
                    _ => debug!("Server {}: Unexpected WAKE for worker {} in state {:?}",
                    self.id, worker_id, current_status),
                }
                debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
                event_type, worker_id);
            },

            e if e == UmcgEventType::Wait as u64 => {
                debug!("!!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!");
                debug!("Server {}: Got explicit WAIT from worker {}", self.id, worker_id);

                // Update status under a short lock
                let previous_status = {
                    if let Some(status) = self.states.get(&worker_id) {
                        let mut status = status.lock().unwrap();
                        let prev = status.clone();
                        debug!("Server {}: Processing explicit WAIT for worker {} in state {:?}",
                        self.id, worker_id, prev);

                        match prev {
                            WorkerStatus::Running => {
                                *status = WorkerStatus::Waiting;
                                let _ = self.worker_queues.running.pop();
                                debug!("Server {}: Transitioned worker {} from Running to Waiting",
                                self.id, worker_id);
                            },
                            _ => {
                                debug!("Server {}: Unexpected WAIT for worker {} in state {:?}",
                                self.id, worker_id, prev);
                            }
                        }
                        prev
                    } else {
                        debug!("Server {}: No state found for worker {}", self.id, worker_id);
                        return Ok(());
                    }
                };

                // Add to pending queue after releasing lock
                match self.worker_queues.pending.push(worker_id) {
                    Ok(_) => debug!("Server {}: Added worker {} to pending queue",
                    self.id, worker_id),
                    Err(_) => debug!("Server {}: Failed to add worker {} to pending queue",
                    self.id, worker_id),
                }

                debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
                event_type, worker_id);
            },

            _ => {
                return Err(ServerError::InvalidWorkerEvent {
                    worker_id: worker_id as usize,
                    event,
                });
            }
        }

        Ok(())
    }

    pub fn shutdown(&self) {
        debug!("Server {}: Initiating shutdown", self.id);
        self.done.store(true, Ordering::Relaxed);

        // Signal all workers to shutdown
        for (worker_id, tx) in self.channels.iter() {
            debug!("Server {}: Sending shutdown to worker {}", self.id, worker_id);
            if tx.send(WorkerTask::Shutdown).is_err() {
                error!("Server {}: Failed to send shutdown to worker {}", self.id, worker_id);
            }
        }
    }
}

struct Executor {
    config: ExecutorConfig,
    worker_handles: Vec<JoinHandle<()>>,
}

impl Executor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            worker_handles: Vec::new(),
        }
    }

    pub fn initialize_first_server_and_setup_workers(
        &mut self,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        worker_queues: Arc<WorkerQueues>,
        manage_tasks: Arc<dyn ManageTask>,
        done: Arc<AtomicBool>,
    ) {
        let server = Server::new(
            0, // First server has id 0
            states,
            channels,
            manage_tasks,
            worker_queues,
            done,
        );

        debug!("Executor: Starting initial server");
        server.start_initial_server();
    }

    pub fn initialize_servers(
        &mut self,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>) {


    }

    pub fn initialize_workers(&mut self) -> (
        Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        Arc<HashMap<u64, Sender<WorkerTask>>>
    ) {
        let worker_states = Arc::new(Mutex::new(HashMap::<u64, Mutex<WorkerStatus>>::new()));
        let worker_channels = Arc::new(Mutex::new(HashMap::<u64, Sender<WorkerTask>>::new()));

        // Spawn all workers
        for worker_id in 0..self.config.worker_count {
            let handle = Worker::spawn(
                worker_id,
                Arc::clone(&worker_states),
                Arc::clone(&worker_channels)
            );

            self.worker_handles.push(handle);
        }

        // Give workers time to initialize
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Create final non-mutex wrapped HashMaps
        let final_states: HashMap<u64, Mutex<WorkerStatus>> =
            worker_states.lock().unwrap()
                .iter()
                .map(|(tid, status)| {
                    let status_value = status.lock().unwrap().clone();
                    (*tid, Mutex::new(status_value))
                })
                .collect();

        let final_channels: HashMap<u64, Sender<WorkerTask>> =
            worker_channels.lock().unwrap()
                .iter()
                .map(|(tid, channel)| {
                    (*tid, channel.clone())
                })
                .collect();

        (Arc::new(final_states), Arc::new(final_channels))
    }
}

struct TaskQueue {
    queue: ArrayQueue<Task>,
}

impl TaskQueue {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
        }
    }
}

trait AddTask: Send + Sync {
    fn add_task(&self, task: Task) -> Result<(), Task>;
}

trait RemoveTask: Send + Sync {
    fn remove_task(&self) -> Option<Task>;
}

trait ManageTask: AddTask + RemoveTask + Send + Sync {}

impl AddTask for TaskQueue {
    fn add_task(&self, task: Task) -> Result<(), Task> {
        self.queue.push(task).map_err(|e| e)
    }
}

impl RemoveTask for TaskQueue {
    fn remove_task(&self) -> Option<Task> {
        self.queue.pop()
    }
}

impl ManageTask for TaskQueue {}

enum TaskState {
    Pending(Task),
    Running {
        worker_tid: i32,
        start_time: SystemTime,
        preempted: bool,
        blocked: bool,
    },
    Completed,
}

pub fn test_basic_worker() {
    const WORKER_COUNT: usize = 2;
    const QUEUE_CAPACITY: usize = 100;

    // Create our task queue and worker queues
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));

    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: 1,
    });

    // Initialize workers first - this will create and spawn worker threads
    let (states, channels) = executor.initialize_workers();

    // Initialize first server with all shared resources
    let server = executor.initialize_first_server_and_setup_workers(
        states,
        channels,
        worker_queues,
        task_queue.clone(),
        Arc::new(AtomicBool::new(false)),
    );

    // Give time for initialization
    thread::sleep(Duration::from_millis(100));

    // Add a test task
    if let Err(_) = task_queue.add_task(Box::new(|| {
        debug!("!!!! Test task: Starting execution !!!!");
        thread::sleep(Duration::from_secs(1));
        debug!("!!!! Test task: Completed !!!!");
    })) {
        error!("Failed to add task to queue");
        return;
    }

    // Let task run
    thread::sleep(Duration::from_secs(10));

    // Initiate shutdown
    // server.shutdown();

    // Give time for shutdown
    thread::sleep(Duration::from_millis(100));

    debug!("Test completed successfully");
}