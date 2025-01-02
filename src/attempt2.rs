use std::collections::{HashMap, HashSet};
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use libc::pid_t;
use uuid::Uuid;
use std::thread::{self, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use log::error;
use crate::{umcg_base, ServerError};
use crate::umcg_base::{UmcgCmd, UmcgEventType, DEBUG_LOGGING, EVENT_BUFFER_SIZE, SYS_UMCG_CTL, UMCG_WAIT_FLAG_INTERRUPTED, UMCG_WORKER_EVENT_MASK, UMCG_WORKER_ID_SHIFT, WORKER_REGISTRATION_TIMEOUT_MS};
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::panic;
use backtrace::Backtrace;

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


type Task = Box<dyn FnOnce() + Send + Sync + 'static>;

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
        cpu_id: usize,
        states: Arc<Mutex<HashMap<u64, Mutex<WorkerStatus>>>>,
        channels: Arc<Mutex<HashMap<u64, Sender<WorkerTask>>>>
    ) -> JoinHandle<()> {
        let (tx, rx) = channel();
        let worker = Worker::new(id, rx);

        let handle = thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };
            let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;  // Shift the tid for storage

            if let Err(e) = set_cpu_affinity(cpu_id) {
                debug!("Failed to set CPU affinity for worker {}: {}", worker_id, e);
            } else {
                debug!("Worker {} CPU affinity set to CPU {}", worker_id, cpu_id);
            }

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
            debug!("Worker {} UMCG registration failed: {}", self.id, reg_result);
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
                        debug!("Worker {} UMCG wait failed: {}", self.id, wait_result);
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
                        debug!("Worker {} UMCG wait failed: {}", self.id, wait_result);
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
            debug!("Worker {} UMCG unregistration failed: {}", self.id, unreg_result);
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

#[derive(Debug)]
struct PendingContextSwitch {
    worker_id: u64,
    events: [u64; EVENT_BUFFER_SIZE],
    start_time: Instant,
    timeout: u64,  // in nanoseconds
}

struct ContextSwitchMetrics {
    success_count: AtomicU64,
    timeout_count: AtomicU64,
    total_time: AtomicU64,    // in nanoseconds
    last_timeout: AtomicU64,  // in nanoseconds
}

impl ContextSwitchMetrics {
    fn new() -> Self {
        Self {
            success_count: AtomicU64::new(0),
            timeout_count: AtomicU64::new(0),
            total_time: AtomicU64::new(0),
            last_timeout: AtomicU64::new(1000),  // Start with 1μs timeout
        }
    }

    fn update_timeout(&self) -> u64 {
        let success = self.success_count.load(Ordering::Relaxed);
        let timeouts = self.timeout_count.load(Ordering::Relaxed);
        let total = success + timeouts;

        if total == 0 {
            return 1000;  // Default 1μs if no data
        }

        // Adjust timeout based on success rate
        let success_rate = success as f64 / total as f64;
        let current = self.last_timeout.load(Ordering::Relaxed);

        let new_timeout = if success_rate > 0.95 {
            // High success rate - try reducing timeout
            (current as f64 * 0.9) as u64
        } else if success_rate < 0.8 {
            // Too many timeouts - increase timeout
            (current as f64 * 1.2) as u64
        } else {
            current
        };

        // Keep timeout between 200ns and 10μs
        let new_timeout = new_timeout.clamp(200, 10_000);
        self.last_timeout.store(new_timeout, Ordering::Relaxed);
        new_timeout
    }
}

struct Server {
    id: usize,
    states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
    channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
    manage_tasks: Arc<dyn ManageTask>,
    worker_queues: Arc<WorkerQueues>,
    metrics: Arc<ContextSwitchMetrics>,
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
            states,
            channels,
            manage_tasks,
            worker_queues,
            metrics: Arc::new(ContextSwitchMetrics::new()),
            done,
        }
    }

    fn start_server(mut self, wait: Arc<AtomicBool>) -> JoinHandle<()> {
        thread::spawn(move || {
            debug!("Starting server {}", self.id);

            // Set CPU affinity for this server thread
            if let Err(e) = set_cpu_affinity(self.id) {
                debug!("Failed to set CPU affinity for server {}: {}", self.id, e);
            } else {
                debug!("Server {} CPU affinity set to CPU {}", self.id, self.id);
            }

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
                    debug!("Server {}: Wait failed during worker registration: {}", self.id, ret);
                    continue;
                }

                debug!("Server {}: before event declaration", self.id);
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
                                debug!("Server {}: Failed to add worker {} to pending queue",
                                self.id, worker_id);
                            }
                            debug!("Server {}: Worker {} status set to waiting and added to pending queue",
                            self.id, worker_id);

                            registered_workers.insert(worker_id);
                        } else {
                            debug!("Server {}: No state found for worker ID {}", self.id, worker_id);
                        }
                    } else {
                        debug!("Server {}: Context switch failed for worker tid {} (id: {}): {}",
                        self.id, worker_tid, worker_id, switch_ret);
                    }
                } else {
                    debug!("Server {}: Unexpected event {} during worker registration",
                    self.id, event_type);
                }
            }

            debug!("Server {}: Worker registration complete, registered {} workers",
            self.id, registered_workers.len());

            while wait.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }

            println!("Server {} workers: {:?} ", self.id, self.channels.keys().collect::<Vec<_>>());
            println!("Server {} workers pending queue: {:?}", self.id, self.worker_queues.pending);
            println!("Server {} workers pending queue: {:?}", self.id, self.worker_queues.pending);

            // Start the event loop
            self.run_event_loop().expect("REASON")
        })
    }


    /*
        let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let abs_timeout = now.as_nanos() as u64 + 1_000_000_000;
     */

    // fn run_event_loop(&self) -> Result<(), ServerError> {
    //     // TODO: ignore wait events that aren't our workers
    //     debug!("Server {}: Starting event loop", self.id);
    //
    //     // Track timeout state and stats
    //     const MIN_TIMEOUT_NS: u64 = 100_000;        // 100μs minimum timeout
    //     const MAX_TIMEOUT_NS: u64 = 1_000_000_000;  // 1s maximum timeout
    //     const BASE_TIMEOUT_NS: u64 = 10_000_000;     // 1ms base timeout
    //     let mut current_timeout = BASE_TIMEOUT_NS;
    //     let mut wait_stats = WaitStats {
    //         total_calls: 0,
    //         eagain_count: 0,
    //         timeout_count: 0,
    //         last_successful_wait: None,
    //         consecutive_failures: 0,
    //     };
    //
    //     // Log initial server state
    //     let thread_id = unsafe { libc::syscall(libc::SYS_gettid) };
    //     println!("Server {} starting on thread {} - Initial Setup", self.id, thread_id);
    //     log_cpu_affinity(self.id);
    //
    //     while !self.done.load(Ordering::Relaxed) {
    //         wait_stats.total_calls += 1;
    //         let loop_start = Instant::now();
    //
    //         // Try to schedule any pending tasks first
    //         let has_pending_tasks = self.manage_tasks.has_pending_tasks();
    //         if has_pending_tasks {
    //             println!("Server {}: Attempting to schedule pending tasks", self.id);
    //             println!("Pre-Schedule State:");
    //             println!("  Pending Workers: {}", self.worker_queues.pending.len());
    //             println!("  Running Workers: {}", self.worker_queues.running.len());
    //             println!("  Has Tasks: {}", has_pending_tasks);
    //         }
    //
    //         // Try to schedule tasks and track result
    //         let schedule_result = self.try_schedule_tasks();
    //         if let Err(e) = &schedule_result {
    //             println!("Server {}: Task scheduling failed: {:?}", self.id, e);
    //         }
    //         schedule_result?;
    //
    //         // Post-scheduling state
    //         if has_pending_tasks {
    //             println!("Post-Schedule State:");
    //             println!("  Pending Workers: {}", self.worker_queues.pending.len());
    //             println!("  Running Workers: {}", self.worker_queues.running.len());
    //         }
    //
    //         let mut events = [0u64; 6];
    //
    //         // Calculate timeout - reset to base timeout if we have work, otherwise increase
    //         let timeout = if self.worker_queues.pending.len() > 0 || has_pending_tasks {
    //             current_timeout = BASE_TIMEOUT_NS;
    //             BASE_TIMEOUT_NS
    //         } else {
    //             current_timeout = (current_timeout * 2).min(MAX_TIMEOUT_NS);
    //             current_timeout
    //         };
    //
    //         // Pre-Wait State Logging
    //         println!("\n=== Server {} Wait #{} ===", self.id, wait_stats.total_calls);
    //         println!("Thread State:");
    //         println!("  Thread ID: {}", thread_id);
    //         println!("  Current CPU: {}", get_current_cpu());
    //
    //         println!("Queue State:");
    //         println!("  Pending Workers: {}", self.worker_queues.pending.len());
    //         println!("  Running Workers: {}", self.worker_queues.running.len());
    //         println!("  Tasks Waiting: {}", has_pending_tasks);
    //
    //         println!("Wait History:");
    //         println!("  Total Calls: {}", wait_stats.total_calls);
    //         println!("  EAGAIN Count: {}", wait_stats.eagain_count);
    //         println!("  Timeout Count: {}", wait_stats.timeout_count);
    //         println!("  Consecutive Failures: {}", wait_stats.consecutive_failures);
    //         println!("  Last Success: {:?} ago", wait_stats.last_successful_wait.map(|t| t.elapsed()));
    //
    //         // Calculate absolute timeout
    //         let abs_timeout = SystemTime::now()
    //             .duration_since(SystemTime::UNIX_EPOCH)
    //             .unwrap()
    //             .as_nanos() as u64 + timeout;
    //
    //         println!("Wait Configuration:");
    //         println!("  Relative Timeout: {}ns", timeout);
    //         println!("  Absolute Timeout: {}", abs_timeout);
    //         println!("  Current Time: {}", SystemTime::now()
    //             .duration_since(SystemTime::UNIX_EPOCH)
    //             .unwrap()
    //             .as_nanos());
    //
    //         // Make the UMCG Wait syscall
    //         let syscall_start = Instant::now();
    //         let ret = umcg_base::sys_umcg_ctl(
    //             0,
    //             UmcgCmd::Wait,
    //             0,
    //             abs_timeout,
    //             Some(&mut events),
    //             6
    //         );
    //         let syscall_duration = syscall_start.elapsed();
    //         let errno = unsafe { *libc::__errno_location() };
    //
    //         // Log Wait results
    //         println!("Wait Result:");
    //         println!("  Return: {}", ret);
    //         println!("  Duration: {:?}", syscall_duration);
    //         println!("  Errno: {} ({})",
    //                  errno,
    //                  std::io::Error::from_raw_os_error(errno));
    //
    //         // Update statistics and handle specific errors
    //         match errno {
    //             0 => {
    //                 wait_stats.consecutive_failures = 0;
    //                 wait_stats.last_successful_wait = Some(Instant::now());
    //             }
    //             libc::EAGAIN => {
    //                 wait_stats.eagain_count += 1;
    //                 wait_stats.consecutive_failures += 1;
    //                 println!("EAGAIN Diagnostics:");
    //                 println!("  Process State: {}", get_process_state());
    //                 println!("  Thread Count: {}", get_thread_count());
    //                 println!("  FD Count: {}", count_open_fds());
    //             }
    //             libc::ETIMEDOUT => {
    //                 wait_stats.timeout_count += 1;
    //                 wait_stats.consecutive_failures += 1;
    //             }
    //             _ => {
    //                 println!("Unexpected errno: {}", errno);
    //                 print_detailed_error_info(errno);
    //             }
    //         }
    //
    //         if ret != 0 && errno != libc::ETIMEDOUT {
    //             println!("Non-timeout error occurred:");
    //             println!("  Error count: {}", wait_stats.consecutive_failures);
    //             println!("  Error code: {} ({})",
    //                      errno,
    //                      std::io::Error::from_raw_os_error(errno));
    //
    //             // Print stack trace for non-timeout errors
    //             println!("Stack trace at error:");
    //             let bt = backtrace::Backtrace::new();
    //             println!("{:?}", bt);
    //
    //             continue;
    //         }
    //
    //         // Process any events we received
    //         let event_count = events.iter().take_while(|&&e| e != 0).count();
    //         if event_count > 0 {
    //             println!("Processing {} events", event_count);
    //             for (i, &event) in events.iter().take_while(|&&e| e != 0).enumerate() {
    //                 println!("Event {}: {:#x}", i, event);
    //                 let event_type = event & umcg_base::UMCG_WORKER_EVENT_MASK;
    //                 let worker_id = event >> umcg_base::UMCG_WORKER_ID_SHIFT;
    //                 if !self.channels.contains_key(&worker_id) {
    //                     continue;
    //                 }
    //                 println!("  Type: {}", event_type);
    //                 println!("  Worker ID: {}", worker_id);
    //
    //                 if let Err(e) = self.handle_event(event) {
    //                     println!("Event handling error: {}", e);
    //                 }
    //             }
    //
    //             // Log queue state after event processing
    //             println!("Post-Event Processing State:");
    //             println!("  Pending Workers: {}", self.worker_queues.pending.len());
    //             println!("  Running Workers: {}", self.worker_queues.running.len());
    //             println!("  Preempted Workers: {}", self.worker_queues.preempted.len());
    //         }
    //
    //         // Log iteration completion
    //         println!("Loop Iteration Complete:");
    //         println!("  Total duration: {:?}", loop_start.elapsed());
    //         println!("================\n");
    //     }
    //
    //     println!("Server {} Event Loop Stats:", self.id);
    //     println!("  Total wait calls: {}", wait_stats.total_calls);
    //     println!("  EAGAIN errors: {}", wait_stats.eagain_count);
    //     println!("  Timeouts: {}", wait_stats.timeout_count);
    //     println!("  Last successful wait: {:?} ago", wait_stats.last_successful_wait.map(|t| t.elapsed()));
    //
    //     debug!("Server {}: Event loop terminated", self.id);
    //     Ok(())
    // }

    fn run_event_loop(&mut self) -> Result<(), ServerError> {
        debug!("Server {}: Starting event loop", self.id);
        let thread_id = unsafe { libc::syscall(libc::SYS_gettid) };
        log_cpu_affinity(self.id);

        while !self.done.load(Ordering::Relaxed) {
            // Try to schedule any pending tasks first
            let has_pending_tasks = self.manage_tasks.has_pending_tasks();
            if has_pending_tasks {
                println!("Server {}: Attempting to schedule pending tasks", self.id);
                println!("Pre-Schedule State:");
                println!("  Pending Workers: {}", self.worker_queues.pending.len());
                println!("  Running Workers: {}", self.worker_queues.running.len());
                println!("  Has Tasks: {}", has_pending_tasks);

                if let Err(e) = self.try_schedule_tasks() {
                    println!("Server {}: Task scheduling failed: {:?}", self.id, e);
                }
            }

            let mut events = [0u64; EVENT_BUFFER_SIZE];
            debug!("Server {}: Waiting for events with retry", self.id);

            // Use umcg_wait_retry instead of direct syscall
            let ret = umcg_base::umcg_wait_retry(0, Some(&mut events), EVENT_BUFFER_SIZE as i32);

            if ret != 0 {
                let errno = unsafe { *libc::__errno_location() };
                if errno != libc::ETIMEDOUT {
                    debug!("Server {}: Wait failed with error {} ({})",
                    self.id, ret, std::io::Error::from_raw_os_error(errno));
                    continue;
                }
            }

            // Process any events we received
            let event_count = events.iter().take_while(|&&e| e != 0).count();
            if event_count > 0 {
                debug!("Server {}: Processing {} events", self.id, event_count);
                for (i, &event) in events.iter().take_while(|&&e| e != 0).enumerate() {
                    let event_type = event & UMCG_WORKER_EVENT_MASK;
                    let worker_id = event >> UMCG_WORKER_ID_SHIFT;
                    debug!("Server {}: Event {}/{}: type {} for worker {} (raw: {:#x})",
                    self.id, i + 1, event_count, event_type, worker_id, event);

                    if let Err(e) = self.handle_event(event) {
                        debug!("Server {}: Event handling error: {}", self.id, e);
                    }
                }
            }
        }
        Ok(())
    }



    // fn try_schedule_tasks(&self) -> Result<(), ServerError> {
    //     debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS TO SCHEDULE !!!!!!!!!!");
    //     debug!("!!!! TRY_SCHEDULE: Checking pending queue size: {} !!!!",
    //        self.worker_queues.pending.len());
    //     debug!("!!!! TRY_SCHEDULE: Checking running queue size: {} !!!!",
    //        self.worker_queues.running.len());
    //
    //     // Try to get a pending worker from the queue
    //     if let Some(worker_id) = self.worker_queues.pending.pop() {
    //         debug!("Server {}: Found pending worker {}", self.id, worker_id);
    //
    //         // Verify worker is actually in waiting state
    //         if let Some(status) = self.states.get(&worker_id) {
    //             let status = status.lock().unwrap();
    //             debug!("Server {}: Worker {} status is {:?}", self.id, worker_id, *status);
    //
    //             if *status == WorkerStatus::Waiting {
    //                 // Try to get a task
    //                 debug!("Server {}: Attempting to get task from queue", self.id);
    //                 match self.manage_tasks.remove_task() {
    //                     Some(task) => {
    //                         debug!("!!!! TRY_SCHEDULE: Found task for worker {} !!!!", worker_id);
    //                         debug!("Server {}: Found task for worker {}", self.id, worker_id);
    //                         if let Some(tx) = self.channels.get(&worker_id) {
    //                             // Update status and move to running queue
    //                             drop(status); // Release the lock before updating
    //                             if let Some(status) = self.states.get(&worker_id) {
    //                                 let mut status = status.lock().unwrap();
    //                                 *status = WorkerStatus::Running;
    //
    //                                 // Add to running queue
    //                                 if self.worker_queues.running.push(worker_id).is_err() {
    //                                     debug!("Server {}: Failed to add worker {} to running queue",
    //                                     self.id, worker_id);
    //                                 }
    //                                 debug!("Server {}: Updated worker {} status to Running", self.id, worker_id);
    //                             }
    //
    //                             // Send task to worker
    //                             if tx.send(WorkerTask::Function(task)).is_ok() {
    //                                 debug!("Server {}: Sent task to worker {}", self.id, worker_id);
    //
    //                                 // Context switch to the worker to let it start the task
    //                                 let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
    //                                 debug!("Server {}: Context switching to worker {} to start task", self.id, worker_id);
    //                                 let switch_ret = unsafe {
    //                                     libc::syscall(
    //                                         SYS_UMCG_CTL as i64,
    //                                         0,
    //                                         UmcgCmd::CtxSwitch as i64,
    //                                         (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,
    //                                         0,
    //                                         switch_events.as_mut_ptr() as i64,
    //                                         EVENT_BUFFER_SIZE as i64
    //                                     )
    //                                 };
    //                                 debug!("Server {}: Context switch returned {} for worker {}",
    //                                 self.id, switch_ret, worker_id);
    //
    //                                 // Process any events from the context switch
    //                                 for &event in switch_events.iter().take_while(|&&e| e != 0) {
    //                                     debug!("Server {}: Got event {} from context switch", self.id, event);
    //                                     self.handle_event(event)?;
    //                                 }
    //                             }
    //                         }
    //                     }
    //                     None => {
    //                         debug!("!!!! TRY_SCHEDULE: No tasks available for worker {} !!!!", worker_id);
    //                         debug!("Server {}: No tasks available, returning worker {} to pending queue",
    //                         self.id, worker_id);
    //                         // No tasks available, put worker back in pending queue
    //                         if self.worker_queues.pending.push(worker_id).is_err() {
    //                             debug!("Server {}: Failed to return worker {} to pending queue",
    //                             self.id, worker_id);
    //                         }
    //                     }
    //                 }
    //             } else {
    //                 debug!("Server {}: Worker {} not in waiting state", self.id, worker_id);
    //                 // Worker not in waiting state, put back in pending queue
    //                 if self.worker_queues.pending.push(worker_id).is_err() {
    //                     debug!("Server {}: Failed to return worker {} to pending queue",
    //                     self.id, worker_id);
    //                 }
    //             }
    //         }
    //     } else {
    //         debug!("Server {}: No pending workers available", self.id);
    //     }
    //     Ok(())
    // }

    fn try_schedule_tasks(&self) -> Result<(), ServerError> {
        // Get a pending worker
        if let Some(worker_id) = self.worker_queues.pending.pop() {
            debug!("Server {}: Found pending worker {}", self.id, worker_id);

            // Scope the status lock
            let should_schedule = {
                if let Some(status) = self.states.get(&worker_id) {
                    let status = status.lock().unwrap();
                    *status == WorkerStatus::Waiting
                } else {
                    false
                }
            };

            if should_schedule {
                // Try to get a task
                if let Some(task) = self.manage_tasks.remove_task() {
                    if let Some(tx) = self.channels.get(&worker_id) {
                        // Send task before status change
                        if tx.send(WorkerTask::Function(task)).is_ok() {
                            // Update status after task is sent
                            if let Some(status) = self.states.get(&worker_id) {
                                let mut status = status.lock().unwrap();
                                *status = WorkerStatus::Running;
                            }

                            // Add to running queue
                            if self.worker_queues.running.push(worker_id).is_err() {
                                debug!("Failed to add worker {} to running queue",
                                worker_id >> UMCG_WORKER_ID_SHIFT);
                                // Revert status if queue update fails
                                if let Some(status) = self.states.get(&worker_id) {
                                    let mut status = status.lock().unwrap();
                                    *status = WorkerStatus::Waiting;
                                }
                                let _ = self.worker_queues.pending.push(worker_id);
                                return Ok(());
                            }

                            // Do context switch after queue update
                            match self.context_switch_to_worker(worker_id) {
                                Ok(_) => {
                                    debug!("Successfully switched to worker {}", worker_id);
                                }
                                Err(e) => {
                                    debug!("Context switch failed: {:?}", e);
                                    // Revert queue and status changes
                                    let _ = self.worker_queues.running.pop();
                                    if let Some(status) = self.states.get(&worker_id) {
                                        let mut status = status.lock().unwrap();
                                        *status = WorkerStatus::Waiting;
                                    }
                                    let _ = self.worker_queues.pending.push(worker_id);
                                }
                            }
                        }
                    }
                } else {
                    // No tasks available, return worker to pending
                    let _ = self.worker_queues.pending.push(worker_id);
                }
            } else {
                // Worker not in waiting state, return to pending
                let _ = self.worker_queues.pending.push(worker_id);
            }
        }
        Ok(())
    }

    // fn handle_event(&self, event: u64) -> Result<(), ServerError> {
    //     let event_type = event & UMCG_WORKER_EVENT_MASK;
    //     let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;
    //
    //     debug!("Server {}: Processing event type {} from worker {} (raw event: {})",
    //     self.id, event_type, worker_id, event);
    //
    //     match event_type {
    //         e if e == UmcgEventType::Block as u64 => {
    //             debug!("Server {}: Worker {} blocked while in running queue", self.id, worker_id);
    //
    //             // Get current status and update it under a short lock
    //             let current_status = {
    //                 if let Some(status) = self.states.get(&worker_id) {
    //                     let mut status = status.lock().unwrap();
    //                     debug!("Server {}: Processing BLOCK for worker {} in state {:?}",
    //                     self.id, worker_id, *status);
    //                     if *status == WorkerStatus::Running {
    //                         *status = WorkerStatus::Blocked;
    //                         debug!("Server {}: Updated worker {} status from Running to Blocked",
    //                         self.id, worker_id);
    //                     }
    //                     status.clone()
    //                 } else {
    //                     debug!("Server {}: No state found for worker {} during BLOCK",
    //                     self.id, worker_id);
    //                     return Ok(());
    //                 }
    //             };
    //             // Worker stays in running queue while blocked
    //             debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
    //             event_type, worker_id);
    //         },
    //
    //         e if e == UmcgEventType::Wake as u64 => {
    //             // Get current status under a short lock
    //             let current_status = {
    //                 if let Some(status) = self.states.get(&worker_id) {
    //                     let status = status.lock().unwrap();
    //                     status.clone()
    //                 } else {
    //                     return Ok(());
    //                 }
    //             };
    //
    //             debug!("Server {}: Processing WAKE for worker {} in state {:?}",
    //             self.id, worker_id, current_status);
    //
    //             match current_status {
    //                 WorkerStatus::Blocked => {
    //                     debug!("Server {}: Worker {} woke up from blocked state, resuming task",
    //                     self.id, worker_id);
    //
    //                     // Update status under a short lock
    //                     if let Some(status) = self.states.get(&worker_id) {
    //                         let mut status = status.lock().unwrap();
    //                         *status = WorkerStatus::Running;
    //                         debug!("Server {}: Updated worker {} status from Blocked to Running",
    //                         self.id, worker_id);
    //                     }
    //
    //                     // Do context switch after releasing lock
    //                     debug!("Server {}: Context switching to unblocked worker {} to resume task",
    //                     self.id, worker_id);
    //                     let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
    //                     let switch_ret = unsafe {
    //                         libc::syscall(
    //                             SYS_UMCG_CTL as i64,
    //                             0,
    //                             UmcgCmd::CtxSwitch as i64,
    //                             (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,
    //                             0,
    //                             switch_events.as_mut_ptr() as i64,
    //                             EVENT_BUFFER_SIZE as i64
    //                         )
    //                     };
    //                     debug!("Server {}: Context switch for unblocked worker {} returned {}",
    //                     self.id, worker_id, switch_ret);
    //
    //                     // Process any events from the context switch
    //                     for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
    //                         debug!("Server {}: Got event {} from unblock context switch",
    //                         self.id, switch_event);
    //                         self.handle_event(switch_event)?;
    //                     }
    //                 },
    //                 _ => debug!("Server {}: Unexpected WAKE for worker {} in state {:?}",
    //                 self.id, worker_id, current_status),
    //             }
    //             debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
    //             event_type, worker_id);
    //         },
    //
    //         e if e == UmcgEventType::Wait as u64 => {
    //             debug!("!!!!!!!!!! EXPLICIT WAIT EVENT - THIS SHOULD BE RARE !!!!!!!!!!");
    //             debug!("Server {}: Got explicit WAIT from worker {}", self.id, worker_id);
    //
    //             // Update status under a short lock
    //             let previous_status = {
    //                 if let Some(status) = self.states.get(&worker_id) {
    //                     let mut status = status.lock().unwrap();
    //                     let prev = status.clone();
    //                     debug!("Server {}: Processing explicit WAIT for worker {} in state {:?}",
    //                     self.id, worker_id, prev);
    //
    //                     match prev {
    //                         WorkerStatus::Running => {
    //                             *status = WorkerStatus::Waiting;
    //                             let _ = self.worker_queues.running.pop();
    //                             debug!("Server {}: Transitioned worker {} from Running to Waiting",
    //                             self.id, worker_id);
    //                         },
    //                         _ => {
    //                             debug!("Server {}: Unexpected WAIT for worker {} in state {:?}",
    //                             self.id, worker_id, prev);
    //                         }
    //                     }
    //                     prev
    //                 } else {
    //                     debug!("Server {}: No state found for worker {}", self.id, worker_id);
    //                     return Ok(());
    //                 }
    //             };
    //
    //             // Add to pending queue after releasing lock
    //             match self.worker_queues.pending.push(worker_id) {
    //                 Ok(_) => debug!("Server {}: Added worker {} to pending queue",
    //                 self.id, worker_id),
    //                 Err(_) => debug!("Server {}: Failed to add worker {} to pending queue",
    //                 self.id, worker_id),
    //             }
    //
    //             debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
    //             event_type, worker_id);
    //         },
    //
    //         _ => {
    //             return Err(ServerError::InvalidWorkerEvent {
    //                 worker_id: worker_id as usize,
    //                 event,
    //             });
    //         }
    //     }
    //
    //     Ok(())
    // }

    // fn handle_event(&self, event: u64) -> Result<(), ServerError> {
    //     let event_type = event & UMCG_WORKER_EVENT_MASK;
    //     let worker_id = event >> UMCG_WORKER_ID_SHIFT;
    //
    //     debug!("Server {}: Processing event {:?} for worker {} (raw event: {:#x})",
    //         self.id,
    //         match event_type {
    //             e if e == UmcgEventType::Block as u64 => "BLOCK",
    //             e if e == UmcgEventType::Wake as u64 => "WAKE",
    //             e if e == UmcgEventType::Wait as u64 => "WAIT",
    //             e if e == UmcgEventType::Exit as u64 => "EXIT",
    //             e if e == UmcgEventType::Timeout as u64 => "TIMEOUT",
    //             e if e == UmcgEventType::Preempt as u64 => "PREEMPT",
    //             _ => "UNKNOWN",
    //         },
    //         worker_id,
    //         event
    //     );
    //
    //     match event_type {
    //         e if e == UmcgEventType::Block as u64 => {
    //             // Update status only, keep in running queue
    //             if let Some(status) = self.states.get(&worker_id) {
    //                 let mut status = status.lock().unwrap();
    //                 if *status == WorkerStatus::Running {
    //                     *status = WorkerStatus::Blocked;
    //                     debug!("Worker {} blocked while running", worker_id);
    //                 }
    //             }
    //         },
    //         e if e == UmcgEventType::Wake as u64 => {
    //             let raw_tid = event >> UMCG_WORKER_ID_SHIFT;  // Get raw tid from event
    //             let worker_id = raw_tid << UMCG_WORKER_ID_SHIFT;  // Shift for HashMap lookup
    //
    //             debug!("Server {}: Processing WAKE event for worker {} (raw tid: {})",
    //     self.id, worker_id, raw_tid);
    //
    //             if let Some(status) = self.states.get(&worker_id) {  // Use shifted ID for lookup
    //                 let mut status = status.lock().unwrap();
    //                 if *status == WorkerStatus::Blocked {
    //                     debug!("Server {}: Worker {} (tid {}) transitioning from Blocked to Running",
    //             self.id, worker_id, raw_tid);
    //                     *status = WorkerStatus::Running;
    //                     drop(status);  // Drop lock before context switch
    //
    //                     // Do context switch with raw tid
    //                     let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
    //                     let switch_ret = unsafe {
    //                         libc::syscall(
    //                             SYS_UMCG_CTL as i64,
    //                             0,
    //                             UmcgCmd::CtxSwitch as i64,
    //                             raw_tid as i32,  // Use raw tid for context switch
    //                             0,
    //                             switch_events.as_mut_ptr() as i64,
    //                             EVENT_BUFFER_SIZE as i64
    //                         )
    //                     };
    //
    //                     if switch_ret == 0 {
    //                         debug!("Server {}: Successfully resumed worker {} (tid {})",
    //                 self.id, worker_id, raw_tid);
    //                     } else {
    //                         // Revert status on failed switch
    //                         if let Some(status) = self.states.get(&worker_id) {
    //                             let mut status = status.lock().unwrap();
    //                             *status = WorkerStatus::Blocked;
    //                         }
    //                     }
    //                 } else {
    //                     debug!("Server {}: Worker {} (tid {}) not blocked (status: {:?})",
    //             self.id, worker_id, raw_tid, *status);
    //                 }
    //             } else {
    //                 debug!("Server {}: No state found for worker {} (tid {})",
    //         self.id, worker_id, raw_tid);
    //             }
    //         },
    //         e if e == UmcgEventType::Wait as u64 => {
    //             // Handle waiting state transition
    //             if let Some(status) = self.states.get(&worker_id) {
    //                 let mut status = status.lock().unwrap();
    //                 if *status == WorkerStatus::Running {
    //                     *status = WorkerStatus::Waiting;
    //                     drop(status); // Release lock before queue operations
    //
    //                     // Remove from running and add to pending atomically
    //                     if self.worker_queues.running.pop().is_some() {
    //                         if self.worker_queues.pending.push(worker_id).is_err() {
    //                             debug!("Failed to move worker {} to pending queue", worker_id);
    //                             // Try to recover
    //                             let _ = self.worker_queues.running.push(worker_id);
    //                             if let Some(status) = self.states.get(&worker_id) {
    //                                 let mut status = status.lock().unwrap();
    //                                 *status = WorkerStatus::Running;
    //                             }
    //                         }
    //                     }
    //                 }
    //             }
    //         },
    //         _ => {
    //             debug!("Unknown event type {} for worker {}", event_type, worker_id);
    //         }
    //     }
    //     Ok(())
    // }
    fn handle_event(&self, event: u64) -> Result<(), ServerError> {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let raw_tid = event >> UMCG_WORKER_ID_SHIFT;
        let worker_id = self.raw_tid_to_worker_id(raw_tid);

        debug!("Server {}: Processing event type {} from worker {} (tid: {}, raw event: {})",
        self.id, event_type, worker_id, raw_tid, event);

        match event_type {
            e if e == UmcgEventType::Block as u64 => {
                debug!("Server {}: Worker {} (tid: {}) blocked while in running queue",
                self.id, worker_id, raw_tid);

                // Get current status and update it under a short lock
                if let Some(mut status) = self.get_worker_state(worker_id) {
                    debug!("Server {}: Processing BLOCK for worker {} (tid: {}) in state {:?}",
                    self.id, worker_id, raw_tid, *status);
                    if *status == WorkerStatus::Running {
                        *status = WorkerStatus::Blocked;
                        debug!("Server {}: Updated worker {} (tid: {}) status from Running to Blocked",
                        self.id, worker_id, raw_tid);
                    }
                } else {
                    debug!("Server {}: No state found for worker {} (tid: {}) during BLOCK",
                    self.id, worker_id, raw_tid);
                    return Ok(());
                }
                // Worker stays in running queue while blocked
                debug!("!!!! EVENT_HANDLER: Completed processing BLOCK for worker {} (tid: {}) !!!!",
                worker_id, raw_tid);
            },

            e if e == UmcgEventType::Wake as u64 => {
                debug!("Server {}: Processing WAKE for worker {} (tid: {})",
                self.id, worker_id, raw_tid);

                if let Some(mut status) = self.get_worker_state(worker_id) {
                    if *status == WorkerStatus::Blocked {
                        debug!("Server {}: Worker {} (tid: {}) woke up from blocked state, resuming task",
                        self.id, worker_id, raw_tid);

                        *status = WorkerStatus::Running;
                        drop(status);  // Release lock before context switch

                        // Do context switch after releasing lock
                        debug!("Server {}: Context switching to unblocked worker {} (tid: {}) to resume task",
                        self.id, worker_id, raw_tid);

                        match self.context_switch_to_worker(worker_id) {
                            Ok(_) => {
                                debug!("Server {}: Successfully resumed worker {} (tid: {}) after wake",
                                self.id, worker_id, raw_tid);
                            }
                            Err(e) => {
                                debug!("Server {}: Context switch failed for worker {} (tid: {}): {:?}",
                                self.id, worker_id, raw_tid, e);
                                // Revert status on failed switch
                                if let Some(mut status) = self.get_worker_state(worker_id) {
                                    *status = WorkerStatus::Blocked;
                                }
                            }
                        }
                    } else {
                        debug!("Server {}: Worker {} (tid: {}) not blocked (status: {:?})",
                        self.id, worker_id, raw_tid, *status);
                    }
                } else {
                    debug!("Server {}: No state found for worker {} (tid: {})",
                    self.id, worker_id, raw_tid);
                }
            },

            e if e == UmcgEventType::Wait as u64 => {
                debug!("Server {}: Got explicit WAIT from worker {} (tid: {})",
                self.id, worker_id, raw_tid);

                if let Some(mut status) = self.get_worker_state(worker_id) {
                    let prev = status.clone();
                    debug!("Server {}: Processing explicit WAIT for worker {} (tid: {}) in state {:?}",
                    self.id, worker_id, raw_tid, prev);

                    match prev {
                        WorkerStatus::Running => {
                            *status = WorkerStatus::Waiting;
                            let _ = self.worker_queues.running.pop();
                            debug!("Server {}: Transitioned worker {} (tid: {}) from Running to Waiting",
                            self.id, worker_id, raw_tid);

                            // Release lock before queue operations
                            drop(status);

                            match self.worker_queues.pending.push(worker_id) {
                                Ok(_) => debug!("Server {}: Added worker {} (tid: {}) to pending queue",
                                self.id, worker_id, raw_tid),
                                Err(_) => debug!("Server {}: Failed to add worker {} (tid: {}) to pending queue",
                                self.id, worker_id, raw_tid),
                            }
                        },
                        _ => {
                            debug!("Server {}: Unexpected WAIT for worker {} (tid: {}) in state {:?}",
                            self.id, worker_id, raw_tid, prev);
                        }
                    }
                } else {
                    debug!("Server {}: No state found for worker {}", self.id, worker_id);
                    return Ok(());
                }

                debug!("!!!! EVENT_HANDLER lame: Completed processing WAIT for worker {} (tid: {}) !!!!",
                worker_id, raw_tid);
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

    fn context_switch_to_worker(&self, worker_id: u64) -> Result<(), ServerError> {
        let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
        debug!("Server {}: Context switching to worker {} to start task", self.id, worker_id);

        let switch_ret = unsafe {
            libc::syscall(
                SYS_UMCG_CTL as i64,
                0,
                UmcgCmd::CtxSwitch as i64,
                (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,  // Convert worker_id to tid
                0,
                switch_events.as_mut_ptr() as i64,
                EVENT_BUFFER_SIZE as i64
            )
        };

        if switch_ret != 0 {
            let errno = unsafe { *libc::__errno_location() };
            debug!("Server {}: Context switch failed: {} (errno: {})",
            self.id, switch_ret, errno);
            return Err(ServerError::ContextSwitchFailed {
                worker_id,
                ret_code: switch_ret as i32,
                errno,
            });
        }

        // Process any events from the context switch
        let event_count = switch_events.iter().take_while(|&&e| e != 0).count();
        debug!("Server {}: Context switch to worker {} returned {} events",
        self.id, worker_id >> UMCG_WORKER_ID_SHIFT, event_count);

        // Process any events from the context switch
        for (idx, &event) in switch_events.iter()
            .take_while(|&&e| e != 0)
            .enumerate()
        {
            let event_type = event & UMCG_WORKER_EVENT_MASK;
            let event_worker = event >> UMCG_WORKER_ID_SHIFT;
            debug!("Server {}: Context switch event {}/{}: type {:?} for worker {} (raw: {:#x})",
            self.id,
            idx + 1,
            event_count,
            match event_type {
                e if e == UmcgEventType::Block as u64 => "BLOCK",
                e if e == UmcgEventType::Wake as u64 => "WAKE",
                e if e == UmcgEventType::Wait as u64 => "WAIT",
                e if e == UmcgEventType::Exit as u64 => "EXIT",
                e if e == UmcgEventType::Timeout as u64 => "TIMEOUT",
                e if e == UmcgEventType::Preempt as u64 => "PREEMPT",
                _ => "UNKNOWN",
            },
            event_worker,
            event
        );
            self.handle_event(event)?;
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
                debug!("Server {}: Failed to send shutdown to worker {}", self.id, worker_id);
            }
        }
    }

    fn get_worker_state(&self, worker_id: u64) -> Option<MutexGuard<WorkerStatus>> {
        // Ensure worker_id is shifted
        let shifted_id = if worker_id & UMCG_WORKER_EVENT_MASK == 0 {
            worker_id
        } else {
            worker_id << UMCG_WORKER_ID_SHIFT
        };

        self.states.get(&shifted_id).and_then(|lock| lock.lock().ok())
    }

    fn raw_tid_to_worker_id(&self, raw_tid: u64) -> u64 {
        raw_tid << UMCG_WORKER_ID_SHIFT
    }

    fn worker_id_to_raw_tid(&self, worker_id: u64) -> i32 {
        (worker_id >> UMCG_WORKER_ID_SHIFT) as i32
    }
}

fn log_cpu_affinity(server_id: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cpu_set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), cpu_set.as_mut_ptr()) == 0 {
            let cpu_set = cpu_set.assume_init();
            let mut allowed_cpus = Vec::new();

            for i in 0..libc::CPU_SETSIZE as i32 {
                if libc::CPU_ISSET(i as usize, &cpu_set) {
                    allowed_cpus.push(i);
                }
            }

            println!("Server {} CPU Affinity:", server_id);
            println!("  Allowed CPUs: {:?}", allowed_cpus);

            // Get current CPU
            let current_cpu = get_current_cpu();
            println!("  Current CPU: {}", current_cpu);

            // Check if current CPU is in allowed set
            if current_cpu >= 0 && libc::CPU_ISSET(current_cpu as usize, &cpu_set) {
                println!("  Status: Running on allowed CPU");
            } else {
                println!("  Status: WARNING - Current CPU not in allowed set!");
            }
        } else {
            println!("Server {}: Failed to get CPU affinity: {}",
                     server_id,
                     std::io::Error::last_os_error());
        }
    }

    #[cfg(not(target_os = "linux"))]
    println!("Server {}: CPU affinity logging not supported on this platform", server_id);
}

struct Executor {
    config: ExecutorConfig,
    worker_handles: Vec<JoinHandle<()>>,
}

fn get_current_cpu() -> i32 {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cpu_set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), cpu_set.as_mut_ptr()) == 0 {
            for i in 0..libc::CPU_SETSIZE as i32 {
                if libc::CPU_ISSET(i as usize, &cpu_set.assume_init()) {
                    return i;
                }
            }
        }
        -1
    }
    #[cfg(not(target_os = "linux"))]
    -1
}

fn get_process_priority() -> i32 {
    unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
}

fn get_process_state() -> String {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(state_line) = status.lines().find(|l| l.starts_with("State:")) {
            return state_line.to_string();
        }
    }
    "Unknown".to_string()
}

fn count_open_fds() -> usize {
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        entries.count()
    } else {
        0
    }
}

fn get_thread_count() -> usize {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(threads_line) = status.lines().find(|l| l.starts_with("Threads:")) {
            if let Some(count) = threads_line.split_whitespace().nth(1) {
                if let Ok(num) = count.parse() {
                    return num;
                }
            }
        }
    }
    0
}

fn print_detailed_error_info(errno: i32) {
    println!("Detailed Error Information:");
    println!("  Error: {} ({})",
             std::io::Error::from_raw_os_error(errno),
             errno);
    println!("  Description: {}", unsafe {
        std::ffi::CStr::from_ptr(libc::strerror(errno))
            .to_string_lossy()
    });

    // Print stack trace
    println!("Stack Trace:");
    let bt = backtrace::Backtrace::new();
    println!("{:?}", bt);
}

struct WaitStats {
    total_calls: usize,
    eagain_count: usize,
    timeout_count: usize,
    last_successful_wait: Option<Instant>,
    consecutive_failures: usize,
}

impl Executor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            worker_handles: Vec::new(),
        }
    }

    pub fn initialize_server_and_setup_workers(
        &mut self,
        server_id: usize,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        worker_queues: Arc<WorkerQueues>,
        manage_tasks: Arc<dyn ManageTask>,
        done: Arc<AtomicBool>,
        wait: Arc<AtomicBool>,
    ) {
        let server = Server::new(
            server_id, // First server has id 0
            states,
            channels,
            manage_tasks,
            worker_queues,
            done,
        );

        debug!("Executor: Starting initial server");
        server.start_server(wait);
    }

    pub fn initialize_workers(&mut self,  cpu_id: usize, worker_count: usize) -> (
        Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        Arc<HashMap<u64, Sender<WorkerTask>>>,
    ) {
        let worker_states = Arc::new(Mutex::new(HashMap::<u64, Mutex<WorkerStatus>>::new()));
        let worker_channels = Arc::new(Mutex::new(HashMap::<u64, Sender<WorkerTask>>::new()));

        // Spawn all workers
        for worker_id in 0..worker_count {
            let handle = Worker::spawn(
                worker_id,
                cpu_id,
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

trait ManageTask: AddTask + RemoveTask + Send + Sync {
    fn has_pending_tasks(&self) -> bool;
}

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

impl ManageTask for TaskQueue {
    fn has_pending_tasks(&self) -> bool {
        self.queue.len() > 0
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


struct TaskStats {
    total_tasks: AtomicUsize,
    completed_tasks: AtomicUsize,
}

impl TaskStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            total_tasks: AtomicUsize::new(0),
            completed_tasks: AtomicUsize::new(0),
        })
    }

    fn register_task(&self) {
        self.total_tasks.fetch_add(1, Ordering::SeqCst);
    }

    fn mark_completed(&self) {
        self.completed_tasks.fetch_add(1, Ordering::SeqCst);
    }

    fn all_tasks_completed(&self) -> bool {
        let completed = self.completed_tasks.load(Ordering::SeqCst);
        let total = self.total_tasks.load(Ordering::SeqCst);
        completed > 0 && completed == total
    }

    fn get_completion_stats(&self) -> (usize, usize) {
        (
            self.completed_tasks.load(Ordering::SeqCst),
            self.total_tasks.load(Ordering::SeqCst)
        )
    }
}

#[derive(Clone)]
struct TaskHandle {
    task_queue: Arc<dyn ManageTask>,
    task_stats: Arc<TaskStats>,
}

impl TaskHandle {
    fn submit<F>(&self, f: Box<F>)
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.task_stats.register_task();
        let stats = self.task_stats.clone();

        let wrapped_task = Box::new(move || {
            f();
            stats.mark_completed();
        });

        if let Err(_) = self.task_queue.add_task(wrapped_task) {
            debug!("Failed to add task to queue");
        }
    }
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

        // Verify the affinity was set
        let mut verify_set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), verify_set.as_mut_ptr()) == 0 {
            let verify_set = verify_set.assume_init();
            if !libc::CPU_ISSET(cpu_id, &verify_set) {
                debug!("WARNING: CPU affinity verification failed for CPU {}", cpu_id);
            }
        }
    }
    Ok(())
}

// pub fn test_basic_worker() {
//     const WORKER_COUNT: usize = 2;
//     const QUEUE_CAPACITY: usize = 100;
//
//     // Create our task queue and worker queues
//     let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
//     let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
//
//     // Create executor with initial configuration
//     let mut executor = Executor::new(ExecutorConfig {
//         worker_count: WORKER_COUNT,
//         server_count: 1,
//     });
//
//     // Initialize workers first - this will create and spawn worker threads
//     let (states, channels) = executor.initialize_workers();
//
//     // Initialize first server with all shared resources
//     let server = executor.initialize_first_server_and_setup_workers(
//         states,
//         channels,
//         worker_queues,
//         task_queue.clone(),
//         Arc::new(AtomicBool::new(false)),
//     );
//
//     // Give time for initialization
//     thread::sleep(Duration::from_millis(100));
//
//     // Add a test task
//     if let Err(_) = task_queue.add_task(Box::new(|| {
//         debug!("!!!! Test task: Starting execution !!!!");
//         thread::sleep(Duration::from_secs(1));
//         debug!("!!!! Test task: Completed !!!!");
//     })) {
//         debug!("Failed to add task to queue");
//         return;
//     }
//
//     // Let task run
//     thread::sleep(Duration::from_secs(10));
//
//     // Initiate shutdown
//     // server.shutdown();
//
//     // Give time for shutdown
//     thread::sleep(Duration::from_millis(100));
//
//     debug!("Test completed successfully");
// }

pub fn run_dynamic_task_attempt2_demo() -> i32 {
    const WORKER_COUNT: usize = 10;
    const SERVER_COUNT: usize = 1;  // Changed this to test multiple servers
    const QUEUE_CAPACITY: usize = 100000;

    // Create task queue and worker queues
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
    let task_stats = TaskStats::new();

    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: SERVER_COUNT,  // Using multiple servers
    });
    let done = Arc::new(AtomicBool::new(false));
    let done_tcp = done.clone(); // Clone for TCP accept loop
    let wait = Arc::new(AtomicBool::new(true));

    for server_id in 0..SERVER_COUNT {
        let (states, channels) = executor.initialize_workers(server_id, WORKER_COUNT);

        executor.initialize_server_and_setup_workers(
            server_id,
            states.clone(),
            channels.clone(),
            worker_queues.clone(),
            task_queue.clone(),
            done.clone(),
            wait.clone(),
        );

        let mut all_workers_ready = false;
        while !all_workers_ready {
            let ready_count = states.values()
                .filter(|state| {
                    let state = state.lock().unwrap();
                    *state == WorkerStatus::Waiting
                })
                .count();
            all_workers_ready = ready_count == WORKER_COUNT;
            if !all_workers_ready {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    wait.store(false, Ordering::SeqCst);

    // Create task handle for submitting tasks
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };

    debug!("Submitting initial tasks...");

    // Submit initial tasks that will spawn child tasks
    for i in 0..30 {
        let parent_task_handle = task_handle.clone();
        let parent_id = i;

        parent_task_handle.clone().submit(Box::new(move || {
            debug!("!!!! Initial task {}: STARTING task !!!!", parent_id);

            debug!("!!!! Initial task {}: ABOUT TO SLEEP !!!!", parent_id);
            thread::sleep(Duration::from_secs(2));
            debug!("!!!! Initial task {}: WOKE UP FROM SLEEP !!!!", parent_id);

            debug!("!!!! Initial task {}: PREPARING to spawn child task !!!!", parent_id);

            // Clone task_handle before moving into the child task closure
            parent_task_handle.submit(Box::new(move || {
                debug!("!!!! Child task of initial task {}: STARTING work !!!!", parent_id);
                thread::sleep(Duration::from_secs(1));
                debug!("!!!! Child task of initial task {}: COMPLETED !!!!", parent_id);
            }));

            debug!("!!!! Initial task {}: COMPLETED !!!!", parent_id);
        }));
    }

    debug!("All tasks submitted, waiting for completion...");

    // Wait for completion with timeout and progress updates
    let start = Instant::now();
    let timeout = Duration::from_secs(30);

    while !task_stats.all_tasks_completed() {
        if start.elapsed() > timeout {
            let (completed, total) = task_stats.get_completion_stats();
            info!("Timeout waiting for tasks to complete! ({}/{} completed)",
                completed, total);
            done.store(true, Ordering::SeqCst);
            return 1;
        }

        if start.elapsed().as_secs() % 5 == 0 {
            let (completed, total) = task_stats.get_completion_stats();
            info!("Progress: {}/{} tasks completed", completed, total);
        }

        thread::sleep(Duration::from_millis(100));
    }

    let (completed, total) = task_stats.get_completion_stats();
    info!("All tasks completed successfully ({}/{})", completed, total);

    // Clean shutdown
    done.store(true, Ordering::SeqCst);
    0
}

struct ServerStats {
    accept_count: AtomicU64,
    accept_wait_time: AtomicU64,  // in microseconds
    processing_time: AtomicU64,   // in microseconds
    queue_wait_time: AtomicU64,   // in microseconds
    completed_requests: AtomicU64,
    current_connections: AtomicU64,
}

impl ServerStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            accept_count: AtomicU64::new(0),
            accept_wait_time: AtomicU64::new(0),
            processing_time: AtomicU64::new(0),
            queue_wait_time: AtomicU64::new(0),
            completed_requests: AtomicU64::new(0),
            current_connections: AtomicU64::new(0),
        })
    }

    fn print_stats(&self) {
        let accepts = self.accept_count.load(Ordering::Relaxed);
        if accepts == 0 { return; }

        println!("=== Performance Statistics ===");
        println!("Total Accepts: {}", accepts);
        println!("Current Connections: {}", self.current_connections.load(Ordering::Relaxed));
        println!("Completed Requests: {}", self.completed_requests.load(Ordering::Relaxed));
        println!("Average Accept Wait: {}μs",
                 self.accept_wait_time.load(Ordering::Relaxed) / accepts);
        println!("Average Processing Time: {}μs",
                 self.processing_time.load(Ordering::Relaxed) / accepts);
        println!("Average Queue Wait: {}μs",
                 self.queue_wait_time.load(Ordering::Relaxed) / accepts);
        println!("===========================\n");
    }
}

pub fn run_echo_server_demo() -> i32 {
    const WORKER_COUNT: usize = 100;  // 500 workers for handling requests
    const QUEUE_CAPACITY: usize = 10000; // Allow for plenty of pending requests

    const SERVER_COUNT : usize = 4;

    // Create task queue and worker queues
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
    let task_stats = TaskStats::new();
    // TODO loop creating workers/server and wait for everything to be ready


    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: 2,
    });

    let wait = Arc::new(AtomicBool::new(true));

    for server_id in 0..SERVER_COUNT {
        let (states, channels) = executor.initialize_workers(server_id, WORKER_COUNT);
        let done = Arc::new(AtomicBool::new(false));
        let done_tcp = done.clone(); // Clone for TCP accept loop


        executor.initialize_server_and_setup_workers(
            server_id,
            states.clone(),
            channels.clone(),
            worker_queues.clone(),
            task_queue.clone(),
            done.clone(),
            wait.clone(),
        );

        let mut all_workers_ready = false;
        while !all_workers_ready {
            let ready_count = states.values()
                .filter(|state| {
                    let state = state.lock().unwrap();
                    *state == WorkerStatus::Waiting
                })
                .count();
            all_workers_ready = ready_count == WORKER_COUNT;
            if !all_workers_ready {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    wait.store(false, Ordering::Relaxed);

    // Initialize workers first

    // Create done flag for shutdown
    let done = Arc::new(AtomicBool::new(false));
    let done_tcp = done.clone(); // Clone for TCP accept loop

    // Create task handle for submitting tasks
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };

    // Create performance stats
    let stats = ServerStats::new();
    let stats_for_accept = stats.clone();
    let stats_for_monitor = stats.clone();

    // Create TCP listener
    let listener = TcpListener::bind("0.0.0.0:8080").expect("Failed to bind to port 8080");
    listener.set_nonblocking(true).expect("Failed to set non-blocking");

    // Submit the TCP accept loop task to a dedicated worker
    let accept_task_handle = task_handle.clone();
    task_handle.submit(Box::new(move || {
        debug!("TCP accept loop starting");
        let mut accept_start = Instant::now();

        while !done_tcp.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    let accept_time = accept_start.elapsed().as_micros() as u64;
                    stats_for_accept.accept_wait_time.fetch_add(accept_time, Ordering::Relaxed);
                    stats_for_accept.accept_count.fetch_add(1, Ordering::Relaxed);
                    stats_for_accept.current_connections.fetch_add(1, Ordering::Relaxed);

                    let stats_for_handler = stats_for_accept.clone();
                    let queued_at = Instant::now();

                    accept_task_handle.submit(Box::new(move || {
                        debug!("inside accept task handle");
                        let queue_time = queued_at.elapsed().as_micros() as u64;
                        stats_for_handler.queue_wait_time.fetch_add(queue_time, Ordering::Relaxed);

                        let process_start = Instant::now();
                        handle_connection(stream);
                        let process_time = process_start.elapsed().as_micros() as u64;

                        stats_for_handler.processing_time.fetch_add(process_time, Ordering::Relaxed);
                        stats_for_handler.completed_requests.fetch_add(1, Ordering::Relaxed);
                        stats_for_handler.current_connections.fetch_sub(1, Ordering::Relaxed);
                    }));

                    accept_start = Instant::now(); // Reset for next accept timing
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => {
                    debug!("Accept error: {}", e);
                    break;
                }
            }
        }
        debug!("TCP accept loop terminated");
    }));

    println!("HTTP server running on 0.0.0.0:8080");

    // Monitor loop
    while !done.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let (completed, total) = task_stats.get_completion_stats();
        stats_for_monitor.print_stats();
    }

    debug!("Server shutdown complete");
    0
}

fn handle_connection(mut stream: TcpStream) {
    let mut buffer = [0; 1024];

    // Add TCP_NODELAY to reduce latency
    if let Err(e) = stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY: {}", e);
    }

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(_n) => {
                let response = "HTTP/1.1 200 OK\r\n\
                               Content-Type: text/plain\r\n\
                               Content-Length: 13\r\n\
                               Connection: close\r\n\
                               \r\n\
                               Hello, World!\n";

                if let Err(e) = stream.write_all(response.as_bytes()) {
                    debug!("Failed to write to socket: {}", e);
                    break;
                }
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_micros(100)); // Reduced sleep time
                continue;
            }
            Err(e) => {
                debug!("Failed to read from socket: {}", e);
                break;
            }
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
}