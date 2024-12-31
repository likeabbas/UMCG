use std::collections::HashMap;
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use libc::pid_t;
use uuid::Uuid;
use std::thread::{self, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use log::error;
use crate::umcg_base;
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
    ) -> JoinHandle<()> {  // No longer need to return the Sender
        let (tx, rx) = channel();
        let worker = Worker::new(id, rx);

        let handle = thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };

            // Store state and channel
            {
                let mut states = states.lock().unwrap();
                states.insert((tid as u64) << UMCG_WORKER_ID_SHIFT, Mutex::new(WorkerStatus::Initializing));

                let mut channels = channels.lock().unwrap();
                channels.insert((tid as u64) << UMCG_WORKER_ID_SHIFT, tx);
            }

            debug!("Worker {} spawned with tid {}", worker.id, tid);

            // Just park the thread for now - we'll implement UMCG registration later
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
    pending: ArrayQueue<usize>,
    running: ArrayQueue<usize>,
    preempted: ArrayQueue<usize>,
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
}

impl Server {
    pub fn new(
        id: usize,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        manage_tasks: Arc<dyn ManageTask>,
        worker_queues: Arc<WorkerQueues>
    ) -> Self {
        Self {
            id,
            states: states.clone(),
            channels: channels.clone(),
            manage_tasks,
            worker_queues: worker_queues.clone(),
        }
    }

    // fn start_initial_server(mut self) -> JoinHandle<()> {
    //     thread::spawn(move || {
    //         // Register server with UMCG
    //         let reg_result = unsafe {
    //             libc::syscall(
    //                 SYS_UMCG_CTL as i64,
    //                 0,
    //                 UmcgCmd::RegisterServer as i64,
    //                 0,
    //                 0,
    //                 std::ptr::null_mut::<u64>() as i64,
    //                 0
    //             )
    //         };
    //
    //         if reg_result == 0 {
    //             debug!("Server {}: UMCG registration complete", self.id);
    //         } else {
    //             debug!("Server {}: UMCG registration failed: {}", self.id, reg_result);
    //             return;
    //         }
    //
    //         // Now wait for UMCG worker registration events
    //         debug!("Server {}: Waiting for worker registration events", self.id);
    //         let mut events = [0u64; EVENT_BUFFER_SIZE];
    //
    //         // Process registration for each worker
    //         for (&worker_tid, status) in self.states.iter() {
    //             let start = SystemTime::now();
    //             let timeout = Duration::from_millis(WORKER_REGISTRATION_TIMEOUT_MS);
    //
    //             loop {
    //                 if start.elapsed().unwrap() > timeout {
    //                     error!("Server {}: Worker registration timeout for worker {}",
    //                     self.id, worker_tid);
    //                     break;
    //                 }
    //
    //                 let ret = unsafe {
    //                     libc::syscall(
    //                         SYS_UMCG_CTL as i64,
    //                         0,
    //                         UmcgCmd::Wait as i64,
    //                         0,
    //                         0,
    //                         events.as_mut_ptr() as i64,
    //                         EVENT_BUFFER_SIZE as i64
    //                     )
    //                 };
    //
    //                 if ret != 0 {
    //                     error!("Server {}: Wait failed during worker registration: {}",
    //                     self.id, ret);
    //                     break;
    //                 }
    //
    //                 for &event in events.iter().take_while(|&&e| e != 0) {
    //                     let event_type = event & UMCG_WORKER_EVENT_MASK;
    //                     let event_worker_tid = (event >> UMCG_WORKER_ID_SHIFT);
    //
    //                     if event_type == UmcgEventType::Wake as u64 && event_worker_tid == worker_tid {
    //                         debug!("Server {}: Got wake event for worker {}", self.id, worker_tid);
    //
    //                         // Context switch to let worker enter wait state
    //                         let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
    //                         let switch_ret = unsafe {
    //                             libc::syscall(
    //                                 SYS_UMCG_CTL as i64,
    //                                 0,
    //                                 UmcgCmd::CtxSwitch as i64,
    //                                 worker_tid as i32,
    //                                 0,
    //                                 switch_events.as_mut_ptr() as i64,
    //                                 EVENT_BUFFER_SIZE as i64
    //                             )
    //                         };
    //
    //                         if switch_ret == 0 {
    //                             // Update worker status to waiting
    //                             if let Some(status) = self.states.get(&worker_tid) {
    //                                 let mut status = status.lock().unwrap();
    //                                 *status = WorkerStatus::Waiting;
    //                                 debug!("Server {}: Worker {} status set to waiting",
    //                                 self.id, worker_tid);
    //                             }
    //                             break;
    //                         } else {
    //                             error!("Server {}: Context switch failed for worker {}: {}",
    //                             self.id, worker_tid, switch_ret);
    //                         }
    //                     }
    //                 }
    //             }
    //         }
    //
    //         debug!("Server {}: Worker registration complete", self.id);
    //     })
    // }

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

            // Process registration for each worker
            for (&worker_id, status) in self.states.iter() {
                let start = SystemTime::now();
                let timeout = Duration::from_millis(WORKER_REGISTRATION_TIMEOUT_MS);

                loop {
                    if start.elapsed().unwrap() > timeout {
                        error!("Server {}: Worker registration timeout for worker {}",
                        self.id, worker_id);
                        break;
                    }

                    let ret = umcg_base::umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);
                    if ret != 0 {
                        error!("Server {}: Wait failed during worker registration: {}",
                        self.id, ret);
                        break;
                    }

                    let event = events[0];
                    let event_type = event & UMCG_WORKER_EVENT_MASK;

                    if event_type == UmcgEventType::Wake as u64 {
                        // Update worker status to waiting
                        if let Some(status) = self.states.get(&worker_id) {
                            let mut status = status.lock().unwrap();
                            *status = WorkerStatus::Waiting;
                            debug!("Server {}: Worker {} status set to waiting",
                            self.id, worker_id);
                        }
                        break;
                    } else {
                        debug!("Server {}: Unexpected event {} during worker {} registration",
                        self.id, event_type, worker_id);
                    }
                }
            }

            debug!("Server {}: Worker registration complete", self.id);
        })
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
    ) {
        let server = Server::new(
            0, // First server has id 0
            states,
            channels,
            manage_tasks,
            worker_queues,
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
        let worker_states = Arc::new(Mutex::new(HashMap::new()));
        let worker_channels = Arc::new(Mutex::new(HashMap::new()));

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
        let final_states = worker_states.lock().unwrap().iter().map(|(&tid, status)| {
            let status_value = status.lock().unwrap().clone();
            ((tid as u64) << UMCG_WORKER_ID_SHIFT, Mutex::new(status_value))
        }).collect();

        let final_channels = worker_channels.lock().unwrap().clone();

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

trait AddTask {
    fn add_task(&self, task: Task) -> Result<(), Task>;
}

trait RemoveTask {
    fn remove_task(&self) -> Option<Task>;
}

trait ManageTask: AddTask + RemoveTask {}

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
    executor.initialize_first_server_and_setup_workers(
        states,
        channels,
        worker_queues,
        task_queue, // No need for casting, TaskQueue implements ManageTask
    );

    // Give some time for everything to initialize
    thread::sleep(Duration::from_secs(2));

    debug!("Test completed successfully");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_worker_initialization() {
        let mut executor = Executor::new(ExecutorConfig {
            worker_count: 2,
            server_count: 1,
        });

        let (worker_states, _channels) = executor.initialize_workers();

        // Give threads a moment to start
        thread::sleep(Duration::from_millis(100));

        // Verify we have the correct number of workers
        assert_eq!(worker_states.len(), 2);

        // Check each worker's status
        for (_pid, status) in worker_states.iter() {
            let status = status.lock().unwrap();
            assert_eq!(*status, WorkerStatus::Initializing);
        }
    }

    #[test]
    fn test_concurrent_status_updates() {
        let mut executor = Executor::new(ExecutorConfig {
            worker_count: 1,
            server_count: 2,
        });

        let (worker_states, _channels) = executor.initialize_workers();

        // Get the worker's tid
        let worker_tid = *worker_states.keys().next().unwrap();

        // Simulate two servers trying to update the same worker
        let states_clone = Arc::clone(&worker_states);
        let handle1 = thread::spawn(move || {
            if let Some(status) = states_clone.get(&worker_tid) {
                let mut status = status.lock().unwrap();
                *status = WorkerStatus::Running;
                thread::sleep(Duration::from_millis(50));  // Hold the lock
            }
        });

        let states_clone = Arc::clone(&worker_states);
        let handle2 = thread::spawn(move || {
            if let Some(status) = states_clone.get(&worker_tid) {
                let mut status = status.lock().unwrap();
                *status = WorkerStatus::Blocked;
            }
        });

        handle1.join().unwrap();
        handle2.join().unwrap();

        // Final state should be Blocked (last update wins)
        let final_status = worker_states.get(&worker_tid).unwrap().lock().unwrap();
        assert_eq!(*final_status, WorkerStatus::Blocked);
    }
}