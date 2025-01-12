use std::collections::{HashMap, HashSet};
use crossbeam::queue::ArrayQueue;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use libc::pid_t;
use uuid::Uuid;
use std::thread::{self, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver, TryRecvError};
use log::error;
use crate::{umcg_base, ServerError};
use crate::umcg_base::{umcg_wait_retry_simple, umcg_wait_retry_timeout, UmcgCmd, UmcgEventType, WaitNoTimeoutResult, WaitResult, DEBUG_LOGGING, EVENT_BUFFER_SIZE, SYS_UMCG_CTL, UMCG_WAIT_FLAG_INTERRUPTED, UMCG_WORKER_EVENT_MASK, UMCG_WORKER_ID_SHIFT, WORKER_REGISTRATION_TIMEOUT_MS};
use std::net::{TcpListener, TcpStream};
use std::io::{Read, Write};
use std::ops::Deref;
use std::os::unix::io::AsRawFd;
use std::panic;
use backtrace::Backtrace;
use dashmap::DashMap;
use ringbuf::producer::Producer;
use ringbuf::{traits::*, HeapRb};

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
             now.subsec_nanos(),
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
        channels: Arc<Mutex<HashMap<u64, Sender<WorkerTask>>>>,
        router: Arc<ServerRouter>,
        server_id: usize,
    ) -> JoinHandle<()> {
        let (tx, rx) = channel();
        let worker = Worker::new(id, rx);

        let handle = thread::spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as pid_t };
            let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;  // Shift the tid for storage

            router.register_worker(worker_id, server_id);

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

struct EventRoutingServer {
    id: usize,
    worker_to_server: HashMap<u64, u64>,
    server_sender: HashMap<u64, ringbuf::HeapProd<u64>>,
    done: Arc<AtomicBool>
}

impl EventRoutingServer {
    pub fn new(
        id: usize,
        worker_to_server: HashMap<u64, u64>,
        server_sender: HashMap<u64, ringbuf::HeapProd<u64>>,
        done: Arc<AtomicBool>
    ) -> Self {
        Self { id, worker_to_server, server_sender, done }
    }

    fn start(mut self, wait: Arc<AtomicBool>) -> JoinHandle<()> {
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

            while !self.done.load(Ordering::Relaxed) {
                let mut events = [0u64; 10];
                log_cpu_affinity(self.id);

                match umcg_wait_retry_simple(
                    0,
                    Some(&mut events),
                    10i32,
                ) {
                    WaitNoTimeoutResult::Events(ret) => {
                        if ret != 0 {
                            let errno = unsafe { *libc::__errno_location() };
                            debug!("Server {}: Wait failed with error {} ({})", self.id, ret, std::io::Error::from_raw_os_error(errno));
                            continue;
                        }
                    }
                    WaitNoTimeoutResult::ProcessForwarded => {
                        continue;
                    }
                    WaitNoTimeoutResult::ResourceBusy => {
                        continue;
                    }
                }

                for (i, &event) in events.iter().take_while(|&&e| e != 0).enumerate() {
                    let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;
                    let server_id = self.worker_to_server[&worker_id];
                    debug!("EventRoutingServer: Routing {:?} event for worker {} to server {} (raw: {:#x})", UmcgEventType::from_u64(event & UMCG_WORKER_EVENT_MASK).unwrap_or(UmcgEventType::Exit), worker_id, server_id, event);

                    if let Some(producer) = self.server_sender.get_mut(&server_id) {
                        match producer.try_push(event) {
                            Ok(()) => {
                                debug!("EventRoutingServer: Successfully pushed event {} (type {}) for worker {} to server {}", i, event & UMCG_WORKER_EVENT_MASK, worker_id, server_id);
                            }
                            Err(push_error) => {
                                debug!("EventRoutingServer: Failed to push event {} (type {}) for worker {} to server {} - buffer full", i, event & UMCG_WORKER_EVENT_MASK, worker_id, server_id);
                                // push_error.0 contains the failed value
                                debug!("EventRoutingServer: Failed event raw value: {:#x}", push_error);
                            }
                        }
                    } else {
                        debug!("EventRoutingServer: No producer found for server {} to handle worker {} event", server_id, worker_id);
                    }
                }
            }
        })
    }
}

struct Server {
    id: usize,
    states: Arc<HashMap<u64, WorkerStatus>>,
    channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
    manage_tasks: Arc<dyn ManageTask>,
    worker_queues: Arc<WorkerQueues>,
    metrics: Arc<ContextSwitchMetrics>,
    router: Arc<ServerRouter>,
    queue: Arc<ServerQueue>,
    event_consumer: ringbuf::HeapCons<u64>,
    done: Arc<AtomicBool>
}

impl Server {
    pub fn new(
        id: usize,
        states: Arc<HashMap<u64, WorkerStatus>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        manage_tasks: Arc<dyn ManageTask>,
        worker_queues: Arc<WorkerQueues>,
        router: Arc<ServerRouter>,
        event_consumer: ringbuf::HeapCons<u64>,
        done: Arc<AtomicBool>,
    ) -> Self {
        let queue = router.get_server_queue(id)
            .expect("Server queue not found");

        debug!(
            "Creating new server with ID: {}. Router pointer = {:p}, queue pointer = {:p}, queue events pointer = {:p}",
            id,
            Arc::as_ptr(&router),
            Arc::as_ptr(&queue),
            &queue.events
        );

        Self {
            id,
            states,
            channels,
            manage_tasks,
            worker_queues,
            metrics: Arc::new(ContextSwitchMetrics::new()),
            router,
            queue,
            event_consumer,
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

    // OG event loop
    // fn run_event_loop(&mut self) -> Result<(), ServerError> {
    //     debug!("Server {}: Starting event loop", self.id);
    //     let thread_id = unsafe { libc::syscall(libc::SYS_gettid) };
    //     log_cpu_affinity(self.id);
    //
    //     let mut base_timeout_ns = 1_000_000; // Start with 1ms
    //     let mut consecutive_busy = 0;
    //     const MAX_BACKOFF_SHIFT: u32 = 6; // Max timeout will be 1ms << 6 = 64ms
    //     const DEBUG_HASH: u64 = 100; // Print debug every 1000 iterations
    //     let mut debug_counter: u64 = 0;
    //
    //     while !self.done.load(Ordering::Relaxed) {
    //         let mut print_debug = false;
    //         if debug_counter % 100000 == 0 {
    //             debug!("Server {} state dump:", self.id);
    //             debug!("  Pending queue size: {}", self.worker_queues.pending.len());
    //             debug!("  Running queue size: {}", self.worker_queues.running.len());
    //             for (worker_id, status) in self.states.iter() {
    //                 let status = status.lock().unwrap();
    //                 debug!("  Worker {}: {:?}", worker_id, *status);
    //             }
    //             debug!("Pending Task Queue size: {}", self.manage_tasks.num_pending_tasks());
    //             print_debug = true;
    //         }
    //
    //         // Increment and possibly reset debug counter
    //         debug_counter = debug_counter.wrapping_add(1);
    //         if debug_counter % DEBUG_HASH == 0 {
    //             debug_counter = 0;
    //         }
    //
    //         if let Err(e) = self.try_schedule_tasks(print_debug) {
    //             debug!("Server {}: Task scheduling failed: {:?}", self.id, e);
    //         }
    //
    //         //TODO - not getting any events for some reason. maybe the hash map is fucked up?
    //         // Process any forwarded events first
    //         while let Some(event) = self.queue.pop_event() {
    //             debug!("Server {}: Queue had event, about to process forwarded event {}",self.id, event);
    //             debug!("Server {}: Processing forwarded event {}", self.id, event);
    //             self.handle_event(event)?;
    //         }
    //
    //         // Try to schedule any pending tasks
    //         // let has_pending_tasks = self.manage_tasks.has_pending_tasks();
    //         // let has_pending_workers = self.worker_queues.pending.len() > 0;
    //         // if has_pending_tasks && has_pending_workers {
    //         //     if let Err(e) = self.try_schedule_tasks() {
    //         //         if debug_counter % DEBUG_HASH == 0 {
    //         //             debug!("Server {}: Task scheduling failed: {:?}", self.id, e);
    //         //         }
    //         //     }
    //         // }
    //
    //         let mut events = [0u64; EVENT_BUFFER_SIZE];
    //         let current_timeout = if consecutive_busy > 0 {
    //             let shift = consecutive_busy.min(MAX_BACKOFF_SHIFT);
    //             base_timeout_ns << shift
    //         } else {
    //             base_timeout_ns
    //         };
    //
    //         match umcg_wait_retry_timeout(
    //             0,
    //             Some(&mut events),
    //             EVENT_BUFFER_SIZE as i32,
    //             current_timeout,
    //             debug_counter,
    //             DEBUG_HASH,
    //         ) {
    //             WaitResult::Events(ret) => {
    //                 consecutive_busy = 0;
    //
    //                 if ret != 0 {
    //                     let errno = unsafe { *libc::__errno_location() };
    //                     if errno != libc::ETIMEDOUT && debug_counter % DEBUG_HASH == 0 {
    //                         debug!("Server {}: Wait failed with error {} ({})",
    //                         self.id, ret, std::io::Error::from_raw_os_error(errno));
    //                         continue;
    //                     }
    //                 }
    //
    //                 let event_count = events.iter().take_while(|&&e| e != 0).count();
    //                 if event_count > 0 {
    //                     if debug_counter % DEBUG_HASH == 0 {
    //                         debug!("Server {}: Processing {} events", self.id, event_count);
    //                     }
    //                     for (i, &event) in events.iter().take_while(|&&e| e != 0).enumerate() {
    //                         let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;
    //
    //                         if !self.channels.contains_key(&worker_id) {
    //                             if debug_counter % DEBUG_HASH == 0 {
    //                                 debug!("Server {}: Forwarding event {} for worker {} to correct server",
    //                                 self.id, event & UMCG_WORKER_EVENT_MASK, worker_id);
    //                             }
    //                             if let Err(e) = self.router.forward_event(worker_id, event) {
    //                                 debug!("Server {}: Failed to forward event: {:?}", self.id, e);
    //                             }
    //                             continue;
    //                         }
    //
    //                         if debug_counter % DEBUG_HASH == 0 {
    //                             debug!("Server {}: Event {}/{}: type {} for worker {} (raw: {:#x})",
    //                             self.id, i + 1, event_count, event & UMCG_WORKER_EVENT_MASK,
    //                             worker_id, event);
    //                         }
    //
    //                         if let Err(e) = self.handle_event(event) {
    //                             if debug_counter % DEBUG_HASH == 0 {
    //                                 debug!("Server {}: Event handling error: {}", self.id, e);
    //                             }
    //                         }
    //                     }
    //                 }
    //             },
    //             WaitResult::ProcessForwarded => {
    //                 consecutive_busy = 0;
    //                 if debug_counter % DEBUG_HASH == 0 {
    //                     debug!("Server {}: Processing forwarded events", self.id);
    //                 }
    //                 continue;
    //             },
    //             WaitResult::Timeout => {
    //                 consecutive_busy = 0;
    //                 if debug_counter % DEBUG_HASH == 0 {
    //                     debug!("Server {}: Wait timed out, checking forwarded events", self.id);
    //                 }
    //                 continue;
    //             },
    //             WaitResult::ResourceBusy => {
    //                 consecutive_busy += 1;
    //                 if debug_counter % DEBUG_HASH == 0 {
    //                     debug!("Server {}: Resource busy, backing off (attempt {})", self.id, consecutive_busy);
    //                 }
    //                 std::thread::sleep(std::time::Duration::from_micros(100));
    //                 continue;
    //             }
    //         }
    //     }
    //     Ok(())
    // }

    fn run_event_loop(&mut self) -> Result<(), ServerError> {
        debug!("Server {}: Starting event loop", self.id);
        let thread_id = unsafe { libc::syscall(libc::SYS_gettid) };
        log_cpu_affinity(self.id);

        const DEBUG_HASH: u64 = 100000;
        const MAX_EVENTS_PER_BATCH: usize = 32; // We can adjust this batch size
        let mut debug_counter: u64 = 0;
        let mut event_buffer = [0u64; MAX_EVENTS_PER_BATCH];

        while !self.done.load(Ordering::Relaxed) {
            let mut print_debug = false;
            if debug_counter % DEBUG_HASH == 0 {
                debug!("Server {} state dump:", self.id);
                debug!("  Pending queue size: {}", self.worker_queues.pending.len());
                debug!("  Running queue size: {}", self.worker_queues.running.len());
                for (worker_id, status) in self.states.iter() {
                    let status = status.lock().unwrap();
                    debug!("  Worker {}: {:?}", worker_id, *status);
                }
                debug!("Pending Task Queue size: {}", self.manage_tasks.num_pending_tasks());
                print_debug = true;
            }

            // Increment and possibly reset debug counter
            debug_counter = debug_counter + 1;
            if debug_counter % DEBUG_HASH == 0 {
                debug_counter = 0;
            }

            // Try to schedule any pending tasks
            if let Err(e) = self.try_schedule_tasks(print_debug) {
                debug!("Server {}: Task scheduling failed: {:?}", self.id, e);
            }

            // Process batch of events from the ring buffer
            let count = self.event_consumer.pop_slice(&mut event_buffer);
            for &event in event_buffer.iter().take(count) {
                let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;

                // if debug_counter % DEBUG_HASH == 0 {
                    debug!("Server {}: Processing event type {} for worker {} (raw: {:#x})",
                    self.id,
                    event & UMCG_WORKER_EVENT_MASK,
                    worker_id,
                    event
                );
                // }

                // Verify this event is for one of our workers
                if !self.channels.contains_key(&worker_id) {
                    debug!("Server {}: Received event for unmanaged worker {}, this should not happen!",
                    self.id, worker_id);
                    continue;
                }

                if let Err(e) = self.handle_event(event) {
                    if debug_counter % DEBUG_HASH == 0 {
                        debug!("Server {}: Event handling error: {}", self.id, e);
                    }
                }
            }
        }
        Ok(())
    }

    fn try_schedule_tasks(&self, print_debug: bool) -> Result<(), ServerError> {
        if print_debug {
            debug!("!!!!!!!!!! SERVER CHECKING FOR TASKS TO SCHEDULE !!!!!!!!!!");
            debug!("!!!! TRY_SCHEDULE: Checking pending queue size: {} !!!!",
            self.worker_queues.pending.len());
            debug!("!!!! TRY_SCHEDULE: Checking running queue size: {} !!!!",
            self.worker_queues.running.len());
        }

        // Keep trying while we have pending workers
        while let Some(worker_id) = self.worker_queues.pending.pop() {
            if print_debug {
                debug!("Server {}: Found pending worker {}", self.id, worker_id);
            }

            // Verify worker is actually in waiting state
            if let Some(status) = self.states.get(&worker_id) {
                let status = status.lock().unwrap();
                if print_debug {
                    debug!("Server {}: Worker {} status is {:?}", self.id, worker_id, *status);
                }

                if *status != WorkerStatus::Waiting {
                    debug!("CRITICAL: Worker {} was in pending queue but had status {:?} - this indicates a state management error",
                    worker_id, *status);
                    if self.worker_queues.pending.push(worker_id).is_err() {
                        debug!("Server {}: Failed to return invalid-state worker {} to pending queue",
                        self.id, worker_id);
                    }
                    continue;
                }

                // Try to get a task
                // debug!("Server {}: Attempting to get task from queue", self.id);
                match self.manage_tasks.remove_task() {
                    Some(tracked_task) => {
                        debug!("!!!! TRY_SCHEDULE: Found task {} ({}) for worker {} created {:?} ago !!!!",
                        tracked_task.id,
                        tracked_task.task_description,
                        worker_id,
                        tracked_task.created_at.elapsed());

                        if let Some(tx) = self.channels.get(&worker_id) {
                            // Update status and move to running queue
                            drop(status);
                            if let Some(status) = self.states.get(&worker_id) {
                                let mut status = status.lock().unwrap();
                                *status = WorkerStatus::Running;

                                if self.worker_queues.running.push(worker_id).is_err() {
                                    error!("Server {}: Failed to add worker {} to running queue for task {}",
                                    self.id, worker_id, tracked_task.id);
                                    *status = WorkerStatus::Waiting;
                                    if self.worker_queues.pending.push(worker_id).is_err() {
                                        error!("Server {}: Failed to return worker {} to pending queue after running queue failure for task {}",
                                        self.id, worker_id, tracked_task.id);
                                    }
                                    continue;
                                }
                                debug!("Server {}: Updated worker {} status to Running for task {}",
                                self.id, worker_id, tracked_task.id);
                            }

                            // Send task to worker
                            if tx.send(WorkerTask::Function(tracked_task.task)).is_ok() {
                                debug!("Server {}: Sent task {} to worker {} after {:?} in queue",
                                self.id, tracked_task.id, worker_id, tracked_task.created_at.elapsed());

                                // Context switch to the worker
                                // let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
                                let mut switch_events = [0u64; 2];
                                debug!("Server {}: Context switching to worker {} to start task {}",
                                self.id, worker_id, tracked_task.id);

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
                                debug!("Server {}: Context switch returned {} for worker {} running task {}",
                                self.id, switch_ret, worker_id, tracked_task.id);

                                // Process context switch events
                                for &event in switch_events.iter().take_while(|&&e| e != 0) {
                                    // for &event in switch_events.iter().take_while(|&&e| e != 0) {
                                    debug!("Server {}: Got event {:?} from context switch while running task {}",
                                    self.id, UmcgEventType::from_u64(event & UMCG_WORKER_EVENT_MASK), tracked_task.id);
                                    self.handle_event(event)?;
                                    // TODO: see if this doesn't work
                                    break;
                                }

           //                      for &event in switch_events.iter().take_while(|&&e| e != 0) {
           //                          let event_type = event & UMCG_WORKER_EVENT_MASK;
           //                          debug!("Server {}: Got event {} (type {}) from context switch while running task {}",
           // self.id, event, event_type, tracked_task.id);
           //
           //                          if event_type != UmcgEventType::Wake as u64 {
           //                              self.handle_event(event)?;
           //                          } else {
           //                              debug!("Server {}: Ignoring non-BLOCK event {} from context switch", self.id, event);
           //                          }
           //                      }
                            } else {
                                error!("Server {}: Failed to send task {} to worker {}",
                                self.id, tracked_task.id, worker_id);
                            }
                        }
                    }
                    None => {
                        if print_debug {
                            debug!("!!!! TRY_SCHEDULE: No tasks available for worker {} !!!!", worker_id);
                            debug!("Server {}: No tasks available, returning worker {} to pending queue",
                            self.id, worker_id);
                        }
                        if self.worker_queues.pending.push(worker_id).is_err() {
                            error!("Server {}: Failed to return worker {} to pending queue",
                            self.id, worker_id);
                        }
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn handle_event(&self, event: u64) -> Result<(), ServerError> {
        let event_type = event & UMCG_WORKER_EVENT_MASK;
        let worker_id = (event >> UMCG_WORKER_ID_SHIFT) << UMCG_WORKER_ID_SHIFT;

        debug!("Server {}: Processing event type {} from worker {} (raw event: {})",
        self.id, event_type, worker_id, event);
        // Add worker state logging
        if let Some(status) = self.states.get(&worker_id) {
            let status = status.lock().unwrap();
            debug!("Server {}: Worker {} current status: {:?}", self.id, worker_id, *status);
        }

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
                        // let mut switch_events = [0u64; EVENT_BUFFER_SIZE];
                        let mut switch_events = [0u64; 2];
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

                        for &event in switch_events.iter().take_while(|&&e| e != 0) {
                            debug!("Server {}: Got event {:?} from context switch in handle_event", self.id, UmcgEventType::from_u64(event & UMCG_WORKER_EVENT_MASK));
                            self.handle_event(event)?;
                            // TODO: see if this doesn't work
                            break;
                        }

                        // Process any events from the context switch
           //              for &switch_event in switch_events.iter().take_while(|&&e| e != 0) {
           //                  let event_type = switch_event & UMCG_WORKER_EVENT_MASK;
           //                  debug!("Server {}: Got event {} (type {}) from unblock context switch",
           // self.id, switch_event, event_type);
           //
           //                  // Only handle BLOCK events (type 1) from context switch
           //                  if event_type != UmcgEventType::Wake as u64 {
           //                      self.handle_event(switch_event)?;
           //                  } else {
           //                      debug!("Server {}: Ignoring non-BLOCK event {} from unblock context switch",
           //     self.id, switch_event);
           //                  }
           //              }

                    },
                    _ => debug!("Server {}: Unexpected WAKE for worker {} in state {:?}",
                    self.id, worker_id, current_status),
                }
                debug!("!!!! EVENT_HANDLER: Completed processing event type {} for worker {} !!!!",
                event_type, worker_id);
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

            let current_cpu = get_current_cpu();
            println!("Server {} CPU Affinity:\nAllowed Cpus: {:?}\nCurrent Cpu: {}", server_id, allowed_cpus, current_cpu);
            // println!("  Allowed CPUs: {:?}", allowed_cpus);

            // Get current CPU
            // println!("  Current CPU: {}", current_cpu);

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
        states: Arc<HashMap<u64, WorkerStatus>>,
        channels: Arc<HashMap<u64, Sender<WorkerTask>>>,
        worker_queues: Arc<WorkerQueues>,
        manage_tasks: Arc<dyn ManageTask>,
        router: Arc<ServerRouter>,
        event_consumer: ringbuf::HeapCons<u64>,
        done: Arc<AtomicBool>,
        wait: Arc<AtomicBool>,
    ) {
        let server = Server::new(
            server_id, // First server has id 0
            states,
            channels,
            manage_tasks,
            worker_queues,
            router,
            event_consumer,
            done,
        );

        debug!("Executor: Starting initial server");
        server.start_server(wait);
    }

    // pub fn initialize_workers(&mut self,  cpu_id: usize, worker_count: usize, router: Arc<ServerRouter>) -> (
    //     Arc<HashMap<u64, Mutex<WorkerStatus>>>,
    //     Arc<HashMap<u64, Sender<WorkerTask>>>,
    // ) {
    //     let worker_states = Arc::new(Mutex::new(HashMap::<u64, Mutex<WorkerStatus>>::new()));
    //     let worker_channels = Arc::new(Mutex::new(HashMap::<u64, Sender<WorkerTask>>::new()));
    //
    //     // Spawn all workers
    //     for worker_id in 0..worker_count {
    //         let handle = Worker::spawn(
    //             worker_id,
    //             cpu_id,
    //             Arc::clone(&worker_states),
    //             Arc::clone(&worker_channels),
    //             router.clone(),
    //             cpu_id,
    //         );
    //
    //         self.worker_handles.push(handle);
    //     }
    //
    //     // Give workers time to initialize
    //     // TODO: change this to a loop
    //     std::thread::sleep(std::time::Duration::from_millis(3000));
    //
    //     // Create final non-mutex wrapped HashMaps
    //     let final_states: HashMap<u64, Mutex<WorkerStatus>> =
    //         worker_states.lock().unwrap()
    //             .iter()
    //             .map(|(tid, status)| {
    //                 let status_value = status.lock().unwrap().clone();
    //                 (*tid, Mutex::new(status_value))
    //             })
    //             .collect();
    //
    //     let final_channels: HashMap<u64, Sender<WorkerTask>> =
    //         worker_channels.lock().unwrap()
    //             .iter()
    //             .map(|(tid, channel)| {
    //                 (*tid, channel.clone())
    //             })
    //             .collect();
    //
    //     (Arc::new(final_states), Arc::new(final_channels))
    // }

    pub fn initialize_workers(&mut self, cpu_id: usize, worker_count: usize, router: Arc<ServerRouter>) -> (
        Arc<HashMap<u64, WorkerStatus>>,
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
                Arc::clone(&worker_channels),
                router.clone(),
                cpu_id,
            );

            self.worker_handles.push(handle);
        }

        // First wait for all workers to register
        loop {
            let states_guard = worker_states.lock().unwrap();
            if states_guard.len() == worker_count {
                break;
            }
            drop(states_guard);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Then wait for all workers to reach Waiting state
        loop {
            let states_guard = worker_states.lock().unwrap();
            if states_guard.iter().all(|(_, status)| {
                matches!(*status.lock().unwrap(), WorkerStatus::Waiting)
            }) {
                break;
            }
            drop(states_guard);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Create final lock-free states HashMap
        let final_states: HashMap<u64, WorkerStatus> =
            worker_states.lock().unwrap()
                .iter()
                .map(|(tid, status)| {
                    (*tid, status.lock().unwrap().clone())
                })
                .collect();

        // Create final channels HashMap
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

struct TrackedTask {
    id: uuid::Uuid,
    task: Task,
    created_at: std::time::Instant,
    // Optional: Store the type name of the closure
    task_description: String,
}

impl TrackedTask {
    fn new(task: Task, task_type: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            task,
            created_at: std::time::Instant::now(),
            task_description: task_type,
        }
    }
}

impl std::fmt::Debug for TrackedTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedTask")
            .field("id", &self.id)
            .field("task", &"[closure]")  // Just print [closure] for the task
            .field("created_at", &self.created_at)
            .field("task_description", &self.task_description)
            .finish()
    }
}

struct TaskQueue {
    queue: ArrayQueue<TrackedTask>,
}

impl TaskQueue {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
        }
    }
}

trait AddTask: Send + Sync {
    fn add_task(&self, task_type: String, task: Task) -> Result<(), Task>;
}

trait RemoveTask: Send + Sync {
    fn remove_task(&self) -> Option<TrackedTask>;
}

trait ManageTask: AddTask + RemoveTask + Send + Sync {
    fn has_pending_tasks(&self) -> bool;

    fn num_pending_tasks(&self) -> usize;
}

impl AddTask for TaskQueue {
    fn add_task(&self, task_type: String, task: Task) -> Result<(), Task> {
        let tracked = TrackedTask::new(task, task_type);
        debug!("Adding task {} of type {} at {:?}",
               tracked.id, tracked.task_description, tracked.created_at);
        self.queue.push(tracked).map_err(|e| e.task)
    }
}

impl RemoveTask for TaskQueue {
    fn remove_task(&self) -> Option<TrackedTask> {
        self.queue.pop()
    }
}

impl ManageTask for TaskQueue {
    fn has_pending_tasks(&self) -> bool {
        self.queue.len() > 0
    }

    fn num_pending_tasks(&self) -> usize {
        self.queue.len()
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
    fn submit<F>(&self, task_description: String, f: Box<F>)
    where
        F: FnOnce() + Send + Sync + 'static
    {
        self.task_stats.register_task();
        let stats = self.task_stats.clone();

        // Create a wrapper that will give us more info about the task
        let created_at = std::time::Instant::now();

        debug!("Creating task of type {} at {:?}", task_description, created_at);

        let wrapped_task = Box::new(move || {
            debug!("Starting execution of task after {:?}", created_at.elapsed());
            f();
            debug!("Completed task after {:?}", created_at.elapsed());
            stats.mark_completed();
        });

        if let Err(_) = self.task_queue.add_task(task_description, wrapped_task) {
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

#[derive(Debug)]
pub struct ServerQueue {
    events: ArrayQueue<u64>,
    server_id: usize,
}

impl ServerQueue {
    pub fn new(server_id: usize, capacity: usize) -> Self {
        Self {
            events: ArrayQueue::new(capacity),
            server_id,
        }
    }

    pub fn push_event(&self, event: u64) -> Result<(), u64> {
        debug!("Queue: Attempting to push event {}", event);
        let result = self.events.push(event);
        debug!("Queue: Pushed returned event {:?}", result);
        Ok(())
    }

    pub fn pop_event(&self) -> Option<u64> {
        let result = self.events.pop();
        // debug!("Queue: Pop returned event: {:?}", result);
        result
    }
}

#[derive(Debug)]
pub struct ServerRouter {
    // Map worker_id to server_id
    worker_assignments: DashMap<u64, usize>,
    // Map server_id to its event queue
    server_queues: HashMap<usize, Arc<ServerQueue>>,
}

impl ServerRouter {
    pub fn new(server_count: usize, queue_capacity: usize) -> Arc<Self> {
        let mut server_queues = HashMap::new();
        for server_id in 0..server_count {
            server_queues.insert(
                server_id,
                Arc::new(ServerQueue::new(server_id, queue_capacity))
            );
        }

        Arc::new(Self {
            worker_assignments: DashMap::new(), // Initialize empty DashMap
            server_queues,
        })
    }

    pub fn register_worker(&self, worker_id: u64, server_id: usize) {
        // Simple insert with DashMap, no need for write lock
        self.worker_assignments.insert(worker_id, server_id);
        debug!("Registered worker {} to server {}", worker_id, server_id);
    }

    pub fn forward_event(&self, worker_id: u64, event: u64) -> Result<(), ServerError> {
        if let Some(server_id) = self.worker_assignments.get(&worker_id).map(|r| *r.value()) {
            if let Some(queue) = self.server_queues.get(&server_id) {
                debug!("About to push event {} to server {}'s queue {:p}, events queue: {:p}",
                   event & UMCG_WORKER_EVENT_MASK, server_id, Arc::as_ptr(queue), &queue.events);
                queue.push_event(event).map_err(|_| ServerError::QueueFull)?;
                debug!("Successfully pushed event {} to server {}'s queue",
                   event & UMCG_WORKER_EVENT_MASK, server_id);
                debug!("Forwarded event {} for worker {} to server {}",
                event & UMCG_WORKER_EVENT_MASK, worker_id, server_id);
                Ok(())
            } else {
                Err(ServerError::NoServerFound)
            }
        } else {
            Err(ServerError::NoServerFound)
        }
    }

    // Optional: If you need to get a server ID directly
    pub fn get_server_for_worker(&self, worker_id: u64) -> Option<usize> {
        self.worker_assignments.get(&worker_id).map(|r| *r.value())
    }

    pub fn get_server_queue(&self, server_id: usize) -> Option<Arc<ServerQueue>> {
        self.server_queues.get(&server_id).cloned()
    }
}

pub fn run_dynamic_task_attempt2_demo() -> i32 {
    const WORKER_COUNT: usize = 2;
    const SERVER_COUNT: usize = 2;  // Changed this to test multiple servers
    const QUEUE_CAPACITY: usize = 100000;
    const RING_SIZE: usize = 1000;

    // Create task queue and worker queues
    let router = ServerRouter::new(SERVER_COUNT, QUEUE_CAPACITY);
    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let task_stats = TaskStats::new();

    // Create executor with initial configuration
    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: SERVER_COUNT,  // Using multiple servers
    });
    let done = Arc::new(AtomicBool::new(false));
    let wait = Arc::new(AtomicBool::new(true));

    let mut server_producers: HashMap<u64, ringbuf::HeapProd<u64>> = HashMap::new();
    let mut server_consumers: HashMap<u64, ringbuf::HeapCons<u64>> = HashMap::new();
    // Initialize ring buffers for each server
    for server_id in 0..SERVER_COUNT {
        let ring = HeapRb::<u64>::new(RING_SIZE);
        let (prod, cons) = ring.split();
        server_producers.insert(server_id as u64, prod);
        server_consumers.insert(server_id as u64, cons);
    }

    let mut worker_to_server: HashMap<u64, u64> = HashMap::new();

    for server_id in 0..SERVER_COUNT {
        let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
        let (states, channels) = executor.initialize_workers(server_id, WORKER_COUNT, router.clone());

        // todo pass EventRoutingServer  consumer for this server id here
        executor.initialize_server_and_setup_workers(
            server_id,
            states.clone(),
            channels.clone(),
            worker_queues.clone(),
            task_queue.clone(),
            router.clone(),
            server_consumers.remove(&(server_id as u64)).unwrap(),
            done.clone(),
            wait.clone(),
        );

        // let mut all_workers_ready = false;
        // while !all_workers_ready {
        //     let ready_workers: Vec<u64> = states.iter()
        //         .filter_map(|(worker_id, state)| {
        //             let state = state.lock().unwrap();
        //             if *state == WorkerStatus::Waiting {
        //                 Some(*worker_id)
        //             } else {
        //                 None
        //             }
        //         })
        //         .collect();
        //
        //     all_workers_ready = ready_workers.len() == WORKER_COUNT;
        //
        //     if all_workers_ready {
        //         // Add worker->server mappings for ready workers
        //         for worker_id in ready_workers {
        //             worker_to_server.insert(worker_id, server_id as u64);
        //         }
        //     } else {
        //         thread::sleep(Duration::from_millis(10));
        //     }
        // }

        // todo when workers ready, populate mut EventRoutingServer hashmap of worker to server id
    }

    let event_routing_server_id = SERVER_COUNT as u64; // equal to server count so it's N + 1
    let event_routing_server = EventRoutingServer::new(
        event_routing_server_id as usize,
        worker_to_server,
        server_producers,
        done.clone()
    );

    let event_routing_handle = event_routing_server.start(wait.clone());

    wait.store(false, Ordering::SeqCst);

    // Create task handle for submitting tasks
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };

    debug!("Submitting initial tasks...");

    // Submit initial tasks that will spawn child tasks
    for i in 0..4 {
        let parent_task_handle = task_handle.clone();
        let parent_id = i;

        parent_task_handle.clone().submit("dynamic_parent_task".parse().unwrap(), Box::new(move || {
            debug!("!!!! Initial task {}: STARTING task !!!!", parent_id);

            debug!("!!!! Initial task {}: ABOUT TO SLEEP !!!!", parent_id);
            thread::sleep(Duration::from_secs(2));
            debug!("!!!! Initial task {}: WOKE UP FROM SLEEP !!!!", parent_id);

            debug!("!!!! Initial task {}: PREPARING to spawn child task !!!!", parent_id);

            // Clone task_handle before moving into the child task closure
            parent_task_handle.submit("dynamic_child_task".parse().unwrap(), Box::new(move || {
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
    const WORKER_COUNT: usize = 1000;
    const QUEUE_CAPACITY: usize = 100000;
    const SERVER_COUNT: usize = 1;
    const ACCEPT_TASK_COUNT: usize = 10;  // Number of TCP accept tasks
    const RING_SIZE: usize = 1000;

    let router = ServerRouter::new(SERVER_COUNT, QUEUE_CAPACITY);

    let task_queue = Arc::new(TaskQueue::new(QUEUE_CAPACITY));
    let task_stats = TaskStats::new();

    let mut executor = Executor::new(ExecutorConfig {
        worker_count: WORKER_COUNT,
        server_count: 2,
    });

    let done = Arc::new(AtomicBool::new(false));
    let wait = Arc::new(AtomicBool::new(false));
    let mut server_producers: HashMap<u64, ringbuf::HeapProd<u64>> = HashMap::new();
    let mut server_consumers: HashMap<u64, ringbuf::HeapCons<u64>> = HashMap::new();
    // Initialize ring buffers for each server
    for server_id in 0..SERVER_COUNT {
        let ring = HeapRb::<u64>::new(RING_SIZE);
        let (prod, cons) = ring.split();
        server_producers.insert(server_id as u64, prod);
        server_consumers.insert(server_id as u64, cons);
    }

    let mut worker_to_server: HashMap<u64, u64> = HashMap::new();

    for server_id in 0..SERVER_COUNT {
        let worker_queues = Arc::new(WorkerQueues::new(WORKER_COUNT));
        let (states, channels) = executor.initialize_workers(server_id, WORKER_COUNT, router.clone());
        let done_tcp = done.clone();

        executor.initialize_server_and_setup_workers(
            server_id,
            states.clone(),
            channels.clone(),
            worker_queues.clone(),
            task_queue.clone(),
            router.clone(),
            server_consumers.remove(&(server_id as u64)).unwrap(),
            done.clone(),
            wait.clone(),
        );

        // let mut all_workers_ready = false;
        // while !all_workers_ready {
        //     let ready_workers: Vec<u64> = states.iter()
        //         .filter_map(|(worker_id, state)| {
        //             let state = state.lock().unwrap();
        //             if *state == WorkerStatus::Waiting {
        //                 Some(*worker_id)
        //             } else {
        //                 None
        //             }
        //         })
        //         .collect();
        //
        //     all_workers_ready = ready_workers.len() == WORKER_COUNT;
        //
        //     if all_workers_ready {
        //         // Add worker->server mappings for ready workers
        //         for worker_id in ready_workers {
        //             worker_to_server.insert(worker_id, server_id as u64);
        //         }
        //     } else {
        //         thread::sleep(Duration::from_millis(10));
        //     }
        // }
    }

    let event_routing_server_id = SERVER_COUNT as u64; // equal to server count so it's N + 1
    let event_routing_server = EventRoutingServer::new(
        event_routing_server_id as usize,
        worker_to_server,
        server_producers,
        done.clone()
    );

    let event_routing_handle = event_routing_server.start(wait.clone());
    wait.store(false, Ordering::Relaxed);

    let done = Arc::new(AtomicBool::new(false));
    let task_handle = TaskHandle {
        task_queue: task_queue.clone(),
        task_stats: task_stats.clone(),
    };

    let stats = ServerStats::new();
    let stats_for_monitor = stats.clone();

    // Create TCP listener
    let listener = TcpListener::bind("0.0.0.0:8080").expect("Failed to bind to port 8080");
    // listener.set_nonblocking(true).expect("Failed to set non-blocking");
    let listener = Arc::new(listener);

    // Create multiple TCP accept tasks
    for task_id in 0..ACCEPT_TASK_COUNT {
        let accept_task_handle = task_handle.clone();
        let listener = listener.clone();
        let done_tcp = done.clone();
        let stats = stats.clone();

        task_handle.submit("tcp_accept_task".parse().unwrap(), Box::new(move || {
            debug!("TCP accept loop {} starting", task_id);
            let mut accept_start = Instant::now();

            while !done_tcp.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let accept_time = accept_start.elapsed().as_micros() as u64;
                        stats.accept_wait_time.fetch_add(accept_time, Ordering::Relaxed);
                        stats.accept_count.fetch_add(1, Ordering::Relaxed);
                        stats.current_connections.fetch_add(1, Ordering::Relaxed);

                        let stats_for_handler = stats.clone();
                        let queued_at = Instant::now();
                        let connection_task_handle = accept_task_handle.clone();

                        connection_task_handle.submit("connection_handle_task".parse().unwrap(), Box::new(move || {
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

                        accept_start = Instant::now();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => {
                        debug!("Accept error in task {}: {}", task_id, e);
                        break;
                    }
                }
            }
            debug!("TCP accept loop {} terminated", task_id);
        }));
    }

    println!("HTTP server running on 0.0.0.0:8080 with {} accept tasks", ACCEPT_TASK_COUNT);

    // Monitor loop
    while !done.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
        let (completed, total) = task_stats.get_completion_stats();
        stats_for_monitor.print_stats();
    }

    debug!("Server shutdown complete");
    0
}

// fn handle_connection(mut stream: TcpStream) {
//     let mut buffer = [0; 1024];
//
//     // Add TCP_NODELAY to reduce latency
//     if let Err(e) = stream.set_nodelay(true) {
//         debug!("Failed to set TCP_NODELAY: {}", e);
//     }
//
//     loop {
//         match stream.read(&mut buffer) {
//             Ok(0) => break,
//             Ok(_n) => {
//                 debug!("Read {} bytes", _n);
//                 let response = "HTTP/1.1 200 OK\r\n\
//                                Content-Type: text/plain\r\n\
//                                Content-Length: 13\r\n\
//                                Connection: close\r\n\
//                                \r\n\
//                                Hello, World!\n";
//
//                 if let Err(e) = stream.write_all(response.as_bytes()) {
//                     debug!("Failed to write to socket: {}", e);
//                     break;
//                 }
//                 break;
//             }
//             Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
//                 thread::sleep(Duration::from_micros(100)); // Reduced sleep time
//                 continue;
//             }
//             Err(e) => {
//                 debug!("Failed to read from socket: {}", e);
//                 break;
//             }
//         }
//     }
//
//     let _ = stream.shutdown(std::net::Shutdown::Both);
// }

fn handle_connection(mut stream: TcpStream) {
    let mut buffer = [0; 1024];

    stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap_or_else(|e| {
        debug!("Failed to set read timeout: {}", e);
    });

    if let Err(e) = stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY: {}", e);
    }

    match stream.read(&mut buffer) {
        Ok(0) => (),
        Ok(_n) => {
            debug!("Read {} bytes", _n);
            let response = "HTTP/1.1 200 OK\r\n\
                           Content-Type: text/plain\r\n\
                           Content-Length: 13\r\n\
                           Connection: close\r\n\
                           \r\n\
                           Hello, World!\n";

            if let Err(e) = stream.write_all(response.as_bytes()) {
                debug!("Failed to write to socket: {}", e);
            }
        }
        Err(e) => {
            debug!("Failed to read from socket: {}", e);
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
}