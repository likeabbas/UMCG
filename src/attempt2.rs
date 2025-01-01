use std::collections::{HashMap, HashSet};
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex};
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
    pending_switches: Arc<ArrayQueue<PendingContextSwitch>>,
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
        pending_switches: Arc<ArrayQueue<PendingContextSwitch>>,
        done: Arc<AtomicBool>,
    ) -> Self {
        Self {
            id,
            states,
            channels,
            manage_tasks,
            worker_queues,
            pending_switches,
            metrics: Arc::new(ContextSwitchMetrics::new()),
            done,
        }
    }

    fn initiate_context_switch(&self, worker_id: u64, timeout: u64) -> Result<(), ServerError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let abs_timeout = now.as_nanos() as u64 + timeout;

        // Create event buffer for this context switch
        let mut events = [0u64; EVENT_BUFFER_SIZE];

        // Initiate non-blocking context switch with event buffer
        let switch_ret = unsafe {
            libc::syscall(
                SYS_UMCG_CTL as i64,
                0,
                UmcgCmd::CtxSwitch as i64,
                (worker_id >> UMCG_WORKER_ID_SHIFT) as i32,
                abs_timeout,
                events.as_mut_ptr() as i64,
                EVENT_BUFFER_SIZE as i64
            )
        };

        if switch_ret == 0 {
            // Create and queue pending switch with its events
            let pending = PendingContextSwitch {
                worker_id,
                events,
                start_time: Instant::now(),
                timeout,
            };

            match self.pending_switches.push(pending) {
                Ok(_) => {
                    debug!("Server {}: Added pending switch for worker {}", self.id, worker_id);
                    Ok(())
                }
                Err(_) => {
                    error!("Server {}: Failed to queue pending switch for worker {}",
                       self.id, worker_id);
                    Err(ServerError::QueueFull)
                }
            }
        } else {
            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::ETIMEDOUT {
                // Timeout is expected, we should check events later
                self.metrics.timeout_count.fetch_add(1, Ordering::Relaxed);
                // Return Ok since timeout isn't a failure
                Ok(())
            } else {
                // Real failure
                error!("Server {}: Context switch syscall failed for worker {} with errno {}",
                   self.id, worker_id, errno);
                Err(ServerError::ContextSwitchFailed)
            }
        }
    }


    fn start_server(self) -> JoinHandle<()> {
        thread::spawn(move || {
            debug!("Starting server {}", self.id);

            // Register with UMCG
            let reg_result = umcg_base::sys_umcg_ctl(
                0,
                UmcgCmd::RegisterServer,
                0,
                0,
                None,
                0
            );
            if reg_result != 0 {
                error!("Server {} registration failed: {}", self.id, reg_result);
                return;
            }
            debug!("Server {}: UMCG registration complete", self.id);

            // Run the event loop
            if let Err(e) = self.run_server() {
                error!("Server {} failed: {}", self.id, e);
            }
        })
    }

    fn run_server(&self) -> Result<(), ServerError> {
        info!("Server {}: Starting up", self.id);

        let reg_result = umcg_base::sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0);
        if reg_result != 0 {
            return Err(ServerError::RegistrationFailed(reg_result));
        }
        info!("Server {}: UMCG registration complete", self.id);

        // Main UMCG event loop - handles both events and task management
        while !self.done.load(Ordering::Relaxed) {
            // Try to schedule any pending tasks first
            debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS BEFORE WAITING !!!!!!!!!!");
            self.try_schedule_tasks()?;

            let mut events = [0u64; EVENT_BUFFER_SIZE];
            debug!("!!!!!!!!!! SERVER EVENT LOOP - WAITING FOR EVENTS (with timeout) !!!!!!!!!!");
            // Add a short timeout (e.g., 100ms) so we don't block forever
            let ret = umcg_base::sys_umcg_ctl(
                0,
                UmcgCmd::Wait,
                0,
                100_000_000, // 100ms in nanoseconds
                Some(&mut events),
                EVENT_BUFFER_SIZE as i32
            );
            debug!("!!!!!!!!!! SERVER EVENT WAIT RETURNED: {} !!!!!!!!!!", ret);

            if ret != 0 && unsafe { *libc::__errno_location() } != libc::ETIMEDOUT {
                error!("Server {} wait error: {}", self.id, ret);
                return Err(ServerError::SystemError(std::io::Error::last_os_error()));
            }

            for &event in events.iter().take_while(|&&e| e != 0) {
                debug!("!!!!!!!!!! SERVER PROCESSING EVENT: {} !!!!!!!!!!", event);
                // Use our existing event handler
                if let Err(e) = self.handle_event(event) {
                    error!("Server {} event handling error: {}", self.id, e);
                }

                // Try to schedule tasks after handling each event too
                // self.try_schedule_tasks()?;
            }
        }

        self.shutdown();
        info!("Server {}: Shutdown complete", self.id);
        Ok(())
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


    /*
        let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let abs_timeout = now.as_nanos() as u64 + 1_000_000_000;
     */
    fn run_event_loop(&self) -> Result<(), ServerError> {
        let mut wait_events = [0u64; EVENT_BUFFER_SIZE];

        while !self.done.load(Ordering::Relaxed) {
            // Try to schedule tasks first
            self.try_schedule_tasks()?;

            // Calculate timeout based on pending switches
            let timeout = if self.pending_switches.is_empty() {
                100_000  // 100μs when no pending switches
            } else {
                1_000    // 1μs when handling switches
            };

            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            let abs_timeout = now.as_nanos() as u64 + timeout;

            // Wait for new events
            let ret = umcg_base::sys_umcg_ctl(
                0,
                UmcgCmd::Wait,
                0,
                abs_timeout,
                Some(&mut wait_events),
                EVENT_BUFFER_SIZE as i32
            );

            // Check return value and errno
            if ret == -1 {
                let errno = unsafe { *libc::__errno_location() };
                match errno {
                    libc::ETIMEDOUT => {
                        debug!("Server {}: Wait timed out", self.id);
                    }
                    libc::EINTR => {
                        // Interrupted, try again
                        debug!("Server {}: Wait interrupted", self.id);
                        continue;
                    }
                    _ => {
                        error!("Server {}: Wait failed with errno {}", self.id, errno);
                        return Err(ServerError::SystemError(std::io::Error::from_raw_os_error(errno)));
                    }
                }
            } else if ret == 0 {
                // Process any Wait events
                for &event in wait_events.iter().take_while(|&&e| e != 0) {
                    self.handle_event(event)?;
                }
            }

            // Process any pending context switches that completed
            let start_process = Instant::now();
            while let Some(switch) = self.pending_switches.pop() {
                let elapsed = start_process.elapsed();

                // Don't spend more than 50μs processing switches
                if elapsed > Duration::from_micros(50) {
                    // Put switch back and process more next iteration
                    if self.pending_switches.push(switch).is_err() {
                        error!("Failed to requeue pending switch");
                    }
                    break;
                }

                if Instant::now().duration_since(switch.start_time).as_nanos() as u64 > switch.timeout {
                    // Context switch timed out - just update metrics
                    self.metrics.timeout_count.fetch_add(1, Ordering::Relaxed);
                    debug!("Server {}: Context switch timed out for worker {}",
                       self.id, switch.worker_id);

                    // The worker's state will be updated when we receive an event
                    // or when scheduling the next task
                } else {
                    debug!("Server {}: Processing context switch events for worker {}",
                       self.id, switch.worker_id);

                    // Process events from the context switch
                    for &event in switch.events.iter().take_while(|&&e| e != 0) {
                        self.handle_event(event)?;
                    }
                    self.metrics.success_count.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Small sleep if no work to avoid spinning
            if self.pending_switches.is_empty() && self.worker_queues.pending.is_empty() {
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
        }

        Ok(())
    }


    fn try_schedule_tasks(&self) -> Result<(), ServerError> {
        let metrics = &self.metrics;
        let mut scheduled = 0;

        while let Some(worker_id) = self.worker_queues.pending.pop() {
            debug!("Server {}: Popped worker {} from pending queue", self.id, worker_id);

            // Check if worker already has a task
            if let Some(status) = self.states.get(&worker_id) {
                let status = status.lock().unwrap();
                if *status == WorkerStatus::Running {
                    debug!("Server {}: Worker {} already running but was in pending queue - discarding",
                       self.id, worker_id);
                    // DO NOT put back in pending - it's running!
                    continue;
                }
            }

            if let Some(task) = self.manage_tasks.remove_task() {
                debug!("Server {}: Found new task for worker {}", self.id, worker_id);

                if let Some(tx) = self.channels.get(&worker_id) {
                    // Update worker status and queues atomically
                    if let Some(status) = self.states.get(&worker_id) {
                        let mut status = status.lock().unwrap();
                        debug!("Server {}: Transitioning worker {} from {:?} to Running",
                           self.id, worker_id, *status);
                        *status = WorkerStatus::Running;

                        // Add to running queue
                        if self.worker_queues.running.push(worker_id).is_ok() {
                            debug!("Server {}: Added worker {} to running queue", self.id, worker_id);

                            // Send task to worker
                            if tx.send(WorkerTask::Function(task)).is_ok() {
                                let timeout = metrics.update_timeout();
                                match self.initiate_context_switch(worker_id, timeout) {
                                    Ok(_) => {
                                        scheduled += 1;
                                        debug!("Server {}: Successfully initiated context switch for worker {}",
                                           self.id, worker_id);
                                    }
                                    Err(ServerError::QueueFull) => {
                                        // Keep state as is, we'll retry the context switch
                                        debug!("Server {}: Context switch queue full for worker {}, will retry",
                                           self.id, worker_id);
                                    }
                                    Err(ServerError::ContextSwitchFailed) => {
                                        // Roll back on actual failures
                                        error!("Server {}: Context switch failed for worker {}, rolling back",
                                           self.id, worker_id);
                                        *status = WorkerStatus::Waiting;
                                        let _ = self.worker_queues.running.pop();
                                        if self.worker_queues.pending.push(worker_id).is_err() {
                                            error!("Server {}: Failed to return worker {} to pending queue after failure",
                                               self.id, worker_id);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Server {}: Unexpected error during context switch for worker {}: {:?}",
                                           self.id, worker_id, e);
                                        *status = WorkerStatus::Waiting;
                                        let _ = self.worker_queues.running.pop();
                                        if self.worker_queues.pending.push(worker_id).is_err() {
                                            error!("Server {}: Failed to return worker {} to pending queue after error",
                                               self.id, worker_id);
                                        }
                                    }
                                }
                            } else {
                                error!("Server {}: Failed to send task to worker {}", self.id, worker_id);
                                // Clean up on send failure
                                *status = WorkerStatus::Waiting;
                                let _ = self.worker_queues.running.pop();
                                if self.worker_queues.pending.push(worker_id).is_err() {
                                    error!("Server {}: Failed to return worker {} to pending queue after send failure",
                                       self.id, worker_id);
                                }
                            }
                        } else {
                            error!("Server {}: Failed to add worker {} to running queue",
                               self.id, worker_id);
                            *status = WorkerStatus::Waiting;
                            if self.worker_queues.pending.push(worker_id).is_err() {
                                error!("Server {}: Failed to return worker {} to pending queue",
                                   self.id, worker_id);
                            }
                        }
                    }
                }
            } else {
                debug!("Server {}: No more tasks available, returning worker {} to pending queue",
                   self.id, worker_id);
                if self.worker_queues.pending.push(worker_id).is_err() {
                    error!("Failed to return worker {} to pending queue", worker_id);
                }
                break;
            }
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
                            debug!("Server {}: Transitioning worker {} from Running to Blocked",
                            self.id, worker_id);
                            *status = WorkerStatus::Blocked;
                        }
                        status.clone()
                    } else {
                        debug!("Server {}: No state found for worker {} during BLOCK",
                        self.id, worker_id);
                        return Ok(());
                    }
                };
                // Worker stays in running queue while blocked - this is important as it's still
                // occupying a server's resources even though it's blocked
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
                        debug!("Server {}: Worker {} woke up from blocked state",
                        self.id, worker_id);

                        // Update status under a short lock
                        if let Some(status) = self.states.get(&worker_id) {
                            let mut status = status.lock().unwrap();
                            debug!("Server {}: Transitioning worker {} from Blocked to Running",
                            self.id, worker_id);
                            *status = WorkerStatus::Running;
                        }

                        // Remove from running queue (was left there while blocked)
                        // and move to pending for rescheduling
                        let _ = self.worker_queues.running.pop();
                        debug!("Server {}: Removed worker {} from running queue", self.id, worker_id);

                        if self.worker_queues.pending.push(worker_id).is_ok() {
                            debug!("Server {}: Added worker {} to pending queue after wake",
                            self.id, worker_id);
                        } else {
                            error!("Server {}: Failed to add worker {} to pending queue after wake",
                            self.id, worker_id);
                        }

                        // Initiate context switch with updated timeout
                        let timeout = self.metrics.update_timeout();
                        self.initiate_context_switch(worker_id, timeout)?;
                    },
                    _ => debug!("Server {}: Unexpected WAKE for worker {} in state {:?}",
                    self.id, worker_id, current_status),
                }
            },

            e if e == UmcgEventType::Wait as u64 => {
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
                                debug!("Server {}: Transitioning worker {} from Running to Waiting",
                                self.id, worker_id);
                                *status = WorkerStatus::Waiting;

                                // Remove from running queue
                                let _ = self.worker_queues.running.pop();
                                debug!("Server {}: Removed worker {} from running queue",
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
                if self.worker_queues.pending.push(worker_id).is_ok() {
                    debug!("Server {}: Added worker {} to pending queue after wait",
                    self.id, worker_id);
                } else {
                    error!("Server {}: Failed to add worker {} to pending queue after wait",
                    self.id, worker_id);
                }
            },

            e if e == UmcgEventType::Exit as u64 => {
                debug!("Server {}: Processing EXIT for worker {}", self.id, worker_id);

                // Update status under a short lock
                if let Some(status) = self.states.get(&worker_id) {
                    let mut status = status.lock().unwrap();
                    debug!("Server {}: Transitioning worker {} from {:?} to Completed",
                    self.id, worker_id, *status);
                    *status = WorkerStatus::Completed;
                }

                // Remove from any queues it might be in
                let _ = self.worker_queues.running.pop();
                debug!("Server {}: Removed worker {} from running queue", self.id, worker_id);
                // Note: Worker should not be in pending queue at exit, but clean up just in case
                while self.worker_queues.pending.pop() == Some(worker_id) {
                    debug!("Server {}: Removed worker {} from pending queue", self.id, worker_id);
                }
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

#[derive(Debug)]
enum ContextSwitchError {
    Timeout,
    QueueFull,
    Failed,
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
        pending_switches: Arc<ArrayQueue<PendingContextSwitch>>,
    ) {
        let server = Server::new(
            0, // First server has id 0
            states,
            channels,
            manage_tasks,
            worker_queues,
            pending_switches,
            done,
        );

        debug!("Executor: Starting initial server");
        server.start_initial_server();
    }

    pub fn initialize_servers(
        &mut self,
        states: Arc<HashMap<u64, Mutex<WorkerStatus>>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        task_queue: Arc<dyn ManageTask>,
        worker_queues: Arc<WorkerQueues>,
        pending_switches: Arc<ArrayQueue<PendingContextSwitch>>,
        done: Arc<AtomicBool>
    ) {
        // Skip server 0 as it's already initialized
        for server_id in 1..self.config.server_count {
            debug!("Executor: Starting server {}", server_id);

            // Create new server
            let server = Server::new(
                server_id,
                states.clone(),
                channels.clone(),
                task_queue.clone(),
                worker_queues.clone(),
                pending_switches.clone(),
                done.clone(),
            );

            // Start the server (just UMCG registration and event loop)
            let handle = server.start_server();

            // Store handle if needed
            self.worker_handles.push(handle);
        }

        debug!("Executor: All {} servers initialized", self.config.server_count);
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
            error!("Failed to add task to queue");
        }
    }
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
//         error!("Failed to add task to queue");
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
    const WORKER_COUNT: usize = 3;
    const SERVER_COUNT: usize = 3;  // Changed this to test multiple servers
    const QUEUE_CAPACITY: usize = 100;

    // Create task queue and worker queues
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
    let task_stats = TaskStats::new();

    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: SERVER_COUNT,  // Using multiple servers
    });

    // Initialize workers first
    let (states, channels) = executor.initialize_workers();

    // Create done flag for shutdown
    let done = Arc::new(AtomicBool::new(false));

    // Create task handle for submitting tasks
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };

    let pending_switches = Arc::new(ArrayQueue::new(WORKER_COUNT));

    // Initialize first server with all shared resources
    executor.initialize_first_server_and_setup_workers(
        states.clone(),  // Clone because we'll use it again
        channels.clone(), // Clone because we'll use it again
        worker_queues.clone(), // Clone because we'll use it again
        task_queue.clone(),
        done.clone(),
        pending_switches.clone()
    );

    // Wait for all workers to be registered
    // We know there should be WORKER_COUNT workers in the waiting state
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
    debug!("All workers registered, starting additional servers");

    // Initialize additional servers
    // executor.initialize_servers(
    //     states,
    //     channels,
    //     task_queue.clone(),
    //     worker_queues,
    //     done.clone()
    // );

    // Give time for initialization
    thread::sleep(Duration::from_millis(100));

    debug!("Submitting initial tasks...");

    // Submit initial tasks that will spawn child tasks
    for i in 0..6 {
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

    // Create task queue and worker queues
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
    let task_stats = TaskStats::new();

    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: 1,
    });

    // Initialize workers first
    let (states, channels) = executor.initialize_workers();

    // Create done flag for shutdown
    let done = Arc::new(AtomicBool::new(false));
    let done_tcp = done.clone(); // Clone for TCP accept loop

    // Create task handle for submitting tasks
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };
    let pending_switches = Arc::new(ArrayQueue::new(WORKER_COUNT));

    // Initialize the server with all shared resources
    executor.initialize_first_server_and_setup_workers(
        states.clone(),
        channels.clone(),
        worker_queues.clone(),
        task_queue.clone(),
        done.clone(),
        pending_switches.clone(),
    );

    // Wait for all workers to be registered
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
    debug!("All workers registered and ready");

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
                    error!("Accept error: {}", e);
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
        error!("Failed to set TCP_NODELAY: {}", e);
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
                    error!("Failed to write to socket: {}", e);
                    break;
                }
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_micros(100)); // Reduced sleep time
                continue;
            }
            Err(e) => {
                error!("Failed to read from socket: {}", e);
                break;
            }
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
}