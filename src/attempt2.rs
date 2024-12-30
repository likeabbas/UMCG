use std::collections::HashMap;
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use libc::pid_t;
use uuid::Uuid;
use std::thread::{self, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use log::error;
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
        states: Arc<Mutex<HashMap<pid_t, Mutex<WorkerStatus>>>>,
        channels: Arc<Mutex<HashMap<pid_t, Sender<WorkerTask>>>>
    ) -> JoinHandle<()> {  // No longer need to return the Sender
        let (tx, rx) = channel();
        let worker = Worker::new(id, rx);

        let handle = thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };

            // Store state and channel
            {
                let mut states = states.lock().unwrap();
                states.insert(tid, Mutex::new(WorkerStatus::Initializing));

                let mut channels = channels.lock().unwrap();
                channels.insert(tid, tx);
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

struct Server {
    id: usize,
    tid: pid_t,
    states: Arc<HashMap<pid_t, Mutex<WorkerStatus>>>,
    channels: Arc<HashMap<pid_t, Sender<WorkerTask>>>,
    pending_workers: Arc<ArrayQueue<pid_t>>,
    running_workers: Arc<ArrayQueue<pid_t>>,
    preempted_workers: Arc<ArrayQueue<pid_t>>,
    manage_tasks: Arc<dyn ManageTask>,
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

    pub fn initialize_workers(&mut self) -> (
        Arc<HashMap<pid_t, Mutex<WorkerStatus>>>,
        Arc<HashMap<pid_t, Sender<WorkerTask>>>
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
            (tid, Mutex::new(status_value))
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
    // Create channels manually for this test
    let (tx, rx) = channel();
    let worker = Worker::new(0, rx);

    // Spawn the worker in a thread
    let handle = thread::spawn(move || {
        debug!("Starting worker thread");
        worker.start();
        debug!("Worker thread completed");
    });

    // Small sleep to ensure worker has started
    thread::sleep(Duration::from_millis(100));

    debug!("Sending shutdown signal");
    tx.send(WorkerTask::Shutdown).expect("Failed to send shutdown");

    // Wait for worker to finish
    handle.join().expect("Worker thread panicked");
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