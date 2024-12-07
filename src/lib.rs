use std::io;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self};
use std::time::{Duration, Instant};

const SYS_UMCG_CTL: i64 = 450;
const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
const MAX_EVENTS: usize = 32;

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

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u64)]
enum UmcgEventType {
    Block = 1,
    Wake,
    Wait,
    Exit,
    Timeout,
    Preempt,
}

impl TryFrom<u64> for UmcgEventType {
    type Error = io::Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(UmcgEventType::Block),
            2 => Ok(UmcgEventType::Wake),
            3 => Ok(UmcgEventType::Wait),
            4 => Ok(UmcgEventType::Exit),
            5 => Ok(UmcgEventType::Timeout),
            6 => Ok(UmcgEventType::Preempt),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid event type")),
        }
    }
}

pub trait LocalState: Send + 'static {
    fn new() -> Self;
}

pub struct Task<T> {
    id: u64,
    work: Box<dyn FnOnce(&mut T) + Send>,
}

enum WorkerMessage<T> {
    Task(Task<T>),
    Shutdown,
}

pub struct WorkerQueue<T> {
    sender: Sender<WorkerMessage<T>>,
}

impl<T> WorkerQueue<T> {
    fn new(sender: Sender<WorkerMessage<T>>) -> Self {
        Self { sender }
    }

    pub fn push(&self, task: Task<T>) -> io::Result<()> {
        self.sender.send(WorkerMessage::Task(task))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

struct WorkerThread<T: LocalState> {
    cpu: usize,
    tid: libc::pid_t,
    worker_id: u64,
    local_state: T,
    receiver: Receiver<WorkerMessage<T>>,
}

impl<T: LocalState> WorkerThread<T> {
    fn new(cpu: usize, receiver: Receiver<WorkerMessage<T>>) -> Self {
        Self {
            cpu,
            tid: 0,
            worker_id: 0,
            local_state: T::new(),
            receiver,
        }
    }

    unsafe fn umcg_syscall(&self, cmd: UmcgCmd, next_tid: i64, abs_timeout: i64, events: Option<&mut [u64]>) -> io::Result<i32> {
        let (events_ptr, events_len) = match events {
            Some(e) => (e.as_mut_ptr() as i64, e.len() as i64),
            None => (0, 0),
        };

        let ret = libc::syscall(
            SYS_UMCG_CTL,
            0i64,
            cmd as i64,
            next_tid,
            abs_timeout,
            events_ptr,
            events_len,
        );

        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as i32)
        }
    }

    fn run(&mut self, running: Arc<AtomicBool>) -> io::Result<()> {
        println!("Worker thread starting initialization");

        unsafe {
            let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(self.cpu, &mut cpuset);
            println!("Worker pinning to CPU {}", self.cpu);
            libc::pthread_setaffinity_np(
                libc::pthread_self(),
                std::mem::size_of::<libc::cpu_set_t>(),
                &cpuset,
            );
        }

        self.tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
        self.worker_id = (self.tid as u64) << UMCG_WORKER_ID_SHIFT;

        println!("Worker {} performing registration with ID {:#x}", self.tid, self.worker_id);
        unsafe {
            match self.umcg_syscall(UmcgCmd::RegisterWorker, 0, self.worker_id as i64, None) {
                Ok(_) => println!("Worker {} registration syscall complete", self.tid),
                Err(e) => {
                    println!("Worker {} registration failed: {}", self.tid, e);
                    return Err(e);
                }
            }
        }

        println!("Worker {} entering task loop", self.tid);

        while running.load(Ordering::SeqCst) {
            match self.receiver.try_recv() {
                Ok(WorkerMessage::Task(task)) => {
                    println!("Worker {} starting task {}", self.tid, task.id);
                    (task.work)(&mut self.local_state);
                    println!("Worker {} completed task {}", self.tid, task.id);

                    unsafe {
                        if let Err(e) = self.umcg_syscall(UmcgCmd::Wait, 0, 0, None) {
                            if e.raw_os_error() != Some(libc::EINTR) {
                                println!("Worker {} yield failed: {}", self.tid, e);
                                break;
                            }
                        }
                    }
                }
                Ok(WorkerMessage::Shutdown) => {
                    println!("Worker {} received shutdown", self.tid);
                    break;
                }
                Err(_) => {
                    if self.receiver.try_recv().is_err() {
                        println!("Worker {} yielding - no tasks", self.tid);
                        unsafe {
                            match self.umcg_syscall(UmcgCmd::Wait, 0, 0, None) {
                                Ok(_) => println!("Worker {} yield complete", self.tid),
                                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                                Err(e) => {
                                    println!("Worker {} yield failed: {}", self.tid, e);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        println!("Worker {} unregistering", self.tid);
        unsafe {
            self.umcg_syscall(UmcgCmd::Unregister, 0, 0, None)?;
        }

        Ok(())
    }
}

pub struct Server<T: LocalState> {
    queues: Arc<Mutex<Vec<WorkerQueue<T>>>>,
    running: Arc<AtomicBool>,
    active_workers: Arc<AtomicUsize>,
}

impl<T: LocalState> Server<T> {
    pub fn new(num_cores: usize) -> Self {
        Self {
            queues: Arc::new(Mutex::new(Vec::with_capacity(num_cores))),
            running: Arc::new(AtomicBool::new(true)),
            active_workers: Arc::new(AtomicUsize::new(0)),
        }
    }

    unsafe fn umcg_syscall(&self, cmd: UmcgCmd, next_tid: i64, abs_timeout: i64, events: Option<&mut [u64]>) -> io::Result<i32> {
        let (events_ptr, events_len) = match events {
            Some(e) => (e.as_mut_ptr() as i64, e.len() as i64),
            None => (0, 0),
        };

        let ret = libc::syscall(
            SYS_UMCG_CTL,
            0i64,
            cmd as i64,
            next_tid,
            abs_timeout,
            events_ptr,
            events_len,
        );

        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as i32)
        }
    }

    pub fn start(&self) -> io::Result<()> {
        println!("Starting UMCG server");

        unsafe {
            self.umcg_syscall(UmcgCmd::RegisterServer, 0, 0, None)?;
        }

        let running = self.running.clone();
        let active_workers = self.active_workers.clone();
        let queues = self.queues.clone();

        // Initialize queues and workers
        let mut queues_guard = queues.lock().unwrap();
        for cpu in 0..num_cpus::get() {
            println!("Creating worker for CPU {}", cpu);

            let (sender, receiver) = channel();
            queues_guard.push(WorkerQueue::new(sender));

            let running = running.clone();
            let active_workers = active_workers.clone();

            let handle = thread::spawn(move || {
                let mut worker = WorkerThread::new(cpu, receiver);
                if let Err(e) = worker.run(running) {
                    println!("Worker failed: {}", e);
                }
                active_workers.fetch_sub(1, Ordering::SeqCst);
            });

            self.active_workers.fetch_add(1, Ordering::SeqCst);
        }
        drop(queues_guard);

        self.process_events()
    }

    fn process_events(&self) -> io::Result<()> {
        let mut events = vec![0u64; MAX_EVENTS];

        while self.running.load(Ordering::SeqCst) {
            println!("Server waiting for events... ({} active workers)",
                     self.active_workers.load(Ordering::SeqCst));

            unsafe {
                match self.umcg_syscall(UmcgCmd::Wait, 0, 0, Some(&mut events)) {
                    Ok(_) => {
                        println!("Raw events received: {:x?}", &events[..8]);
                        for &event in events.iter() {
                            if event == 0 {
                                break;
                            }

                            let worker_id = event & !UMCG_WORKER_EVENT_MASK;
                            let event_type = UmcgEventType::try_from(event & UMCG_WORKER_EVENT_MASK)?;
                            let tid = (worker_id >> UMCG_WORKER_ID_SHIFT) as libc::pid_t;

                            println!("Raw event {:#x}: worker_id {:#x}, type {:?}, tid {}",
                                     event, worker_id, event_type, tid);

                            match event_type {
                                UmcgEventType::Block => {
                                    println!("Worker {} blocked - waiting for unblock", tid);
                                    let mut switch_events = [0u64; 2];
                                    if let Err(e) = self.umcg_syscall(UmcgCmd::Wait, tid as i64, 0, Some(&mut switch_events)) {
                                        if e.raw_os_error() != Some(libc::EINTR) {
                                            println!("Failed waiting for blocked worker {}: {}", tid, e);
                                        }
                                    }
                                }
                                UmcgEventType::Wake => {
                                    println!("Worker {} woke up - switching context", tid);
                                    let mut switch_events = [0u64; 2];
                                    if let Err(e) = self.umcg_syscall(UmcgCmd::CtxSwitch, tid as i64, 0, Some(&mut switch_events)) {
                                        println!("Failed to switch to worker {}: {}", tid, e);
                                        continue;
                                    }
                                }
                                UmcgEventType::Wait => {
                                    println!("Worker {} waiting - checking for tasks", tid);
                                    if let Err(e) = self.umcg_syscall(UmcgCmd::CtxSwitch, tid as i64, 0, None) {
                                        println!("Failed to schedule worker {}: {}", tid, e);
                                    }
                                }
                                UmcgEventType::Exit => {
                                    println!("Worker {} exiting - cleaning up", tid);
                                    self.active_workers.fetch_sub(1, Ordering::SeqCst);
                                }
                                UmcgEventType::Timeout => {
                                    println!("Worker {} timed out - resuming execution", tid);
                                    if let Err(e) = self.umcg_syscall(UmcgCmd::CtxSwitch, tid as i64, 0, None) {
                                        println!("Failed to resume timed out worker {}: {}", tid, e);
                                    }
                                }
                                UmcgEventType::Preempt => {
                                    println!("Worker {} was preempted - rescheduling", tid);
                                    let mut preempt_events = [0u64; 2];
                                    if let Err(e) = self.umcg_syscall(UmcgCmd::CtxSwitch, tid as i64, 0, Some(&mut preempt_events)) {
                                        println!("Failed to reschedule preempted worker {}: {}", tid, e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                        println!("Server wait interrupted - retrying");
                        continue;
                    }
                    Err(e) => {
                        println!("Server wait failed: {}", e);
                        return Err(e);
                    }
                }
            }

            if !self.running.load(Ordering::SeqCst) && self.active_workers.load(Ordering::SeqCst) == 0 {
                println!("Shutdown complete - no more active workers");
                break;
            }
        }

        Ok(())
    }

    pub fn submit_work(&self, core: usize, work: impl FnOnce(&mut T) + Send + 'static) -> io::Result<()> {
        let queues = self.queues.lock().unwrap();
        if core >= queues.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid core number",
            ));
        }

        println!("Submitting work to core {}", core);

        let task = Task {
            id: core as u64,
            work: Box::new(work),
        };

        queues[core].push(task)?;
        drop(queues);

        // Wake any idle workers after submitting work
        unsafe {
            if let Err(e) = self.umcg_syscall(UmcgCmd::Wake, 0, 0, None) {
                if e.raw_os_error() != Some(libc::EAGAIN) {
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    pub fn shutdown(&self) {
        println!("Initiating shutdown");
        self.running.store(false, Ordering::SeqCst);

        // Signal shutdown to all workers
        let queues = self.queues.lock().unwrap();
        for queue in queues.iter() {
            let _ = queue.sender.send(WorkerMessage::Shutdown);
        }
        drop(queues);

        // Wake up any sleeping workers to process shutdown
        unsafe {
            let _ = self.umcg_syscall(UmcgCmd::Wake, 0, 0, None);
        }
    }
}

pub fn example_basic() -> io::Result<()> {
    struct MyState {
        counter: usize,
    }

    impl LocalState for MyState {
        fn new() -> Self {
            Self { counter: 0 }
        }
    }

    let server = Arc::new(Server::<MyState>::new(1));
    let server_clone = server.clone();

    println!("Created server");

    let server_thread = thread::spawn(move || {
        println!("Starting server thread");
        if let Err(e) = server_clone.start() {
            println!("Server error: {}", e);
        }
    });

    // Give the server time to initialize
    thread::sleep(Duration::from_millis(500));

    println!("Submitting work to server");
    server.submit_work(0, |state| {
        state.counter += 1;
        println!("Task executed, counter: {}", state.counter);
    })?;

    println!("Work submitted, waiting for completion");
    thread::sleep(Duration::from_secs(1));

    server.shutdown();
    server_thread.join().unwrap();

    Ok(())
}


pub fn example() -> io::Result<()> {
    struct MyState {
        counter: usize,
    }

    impl LocalState for MyState {
        fn new() -> Self {
            Self { counter: 0 }
        }
    }

    let mut server = Server::<MyState>::new(num_cpus::get());
    server.start()?;

    // Submit multiple tasks
    for i in 0..10 {
        let core = i % num_cpus::get();
        server.submit_work(core, move |state| {
            state.counter += 1;
            println!("Task {} executed on core {}, counter: {}", i, core, state.counter);
        })?;
        thread::sleep(Duration::from_millis(10));
    }

    // Let tasks complete
    thread::sleep(Duration::from_secs(2));

    server.shutdown();
    Ok(())
}

pub fn example_complex() -> std::io::Result<()> {
    struct ComplexState {
        counter: usize,
        total_work_time: Duration,
        tasks_completed: usize,
        last_task_timestamp: Option<Instant>,
    }

    impl LocalState for ComplexState {
        fn new() -> Self {
            Self {
                counter: 0,
                total_work_time: Duration::new(0, 0),
                tasks_completed: 0,
                last_task_timestamp: None,
            }
        }
    }

    let num_cores = num_cpus::get().min(4);
    let server = Arc::new(Server::<ComplexState>::new(num_cores));
    let server_clone = server.clone();

    println!("Created server with {} workers", num_cores);

    let server_thread = thread::spawn(move || {
        println!("Starting server thread");
        if let Err(e) = server_clone.start() {
            println!("Server error: {}", e);
        }
    });

    thread::sleep(Duration::from_millis(500));

    // Define workload types using a simple deterministic pattern instead of random
    #[derive(Clone, Copy)]
    enum WorkloadType {
        Cpu,
        Io,
        Mixed,
    }

    let total_tasks = 20;
    println!("Submitting {} tasks across {} cores", total_tasks, num_cores);

    for i in 0..total_tasks {
        let core = i % num_cores;

        // Determine workload type deterministically based on task number
        let workload_type = match i % 3 {
            0 => WorkloadType::Cpu,
            1 => WorkloadType::Io,
            _ => WorkloadType::Mixed,
        };

        println!("Submitting task {} to core {}", i, core);

        server.submit_work(core, move |state| {
            println!("Starting task {} on core {}", i, core);

            match workload_type {
                WorkloadType::Cpu => {
                    state.last_task_timestamp = Some(Instant::now());
                    for i in 0..1_000_000 {
                        state.counter = state.counter.wrapping_add(i);
                    }
                    state.tasks_completed += 1;
                    if let Some(start) = state.last_task_timestamp {
                        state.total_work_time += start.elapsed();
                    }
                    println!("CPU-intensive task completed on worker, counter: {}", state.counter);
                },
                WorkloadType::Io => {
                    state.last_task_timestamp = Some(Instant::now());
                    thread::sleep(Duration::from_millis(100));
                    state.tasks_completed += 1;
                    if let Some(start) = state.last_task_timestamp {
                        state.total_work_time += start.elapsed();
                    }
                    println!("I/O-simulating task completed on worker, total tasks: {}", state.tasks_completed);
                },
                WorkloadType::Mixed => {
                    state.last_task_timestamp = Some(Instant::now());
                    for i in 0..500_000 {
                        state.counter = state.counter.wrapping_add(i);
                    }
                    thread::sleep(Duration::from_millis(50));
                    state.tasks_completed += 1;
                    if let Some(start) = state.last_task_timestamp {
                        state.total_work_time += start.elapsed();
                    }
                    println!("Mixed task completed on worker, counter: {}, tasks: {}",
                             state.counter, state.tasks_completed);
                }
            }
        })?;

        // Use deterministic delay based on task number instead of random
        let delay = (i % 5) * 20; // 0, 20, 40, 60, or 80ms
        thread::sleep(Duration::from_millis(delay as u64));
    }

    println!("All tasks submitted, waiting for completion");
    thread::sleep(Duration::from_secs(3));

    println!("Initiating shutdown");
    server.shutdown();
    server_thread.join().unwrap();

    Ok(())
}

// Add this to main.rs or lib.rs
pub fn run_examples() -> io::Result<()> {
    println!("Running basic example...");
    example_basic()?;

    println!("\nRunning complex multi-worker example...");
    example_complex()?;

    Ok(())
}

fn main() -> io::Result<()> {
    example()
}

// use std::io;
// use std::sync::mpsc::{channel, Sender, Receiver};
// use std::collections::{VecDeque, HashMap};
// use std::sync::atomic::{AtomicU64, Ordering};
//
// // Syscall definitions
// const SYS_UMCG: i32 = 450; // Adjust for your system
//
// #[repr(u64)]
// #[derive(Debug, Clone, Copy)]
// enum UmcgCmd {
//     RegisterWorker = 1,
//     RegisterServer = 2,
//     Unregister = 3,
//     Wake = 4,
//     Wait = 5,
//     CtxSwitch = 6,
// }
//
// const UMCG_WORKER_ID_ALIGNMENT: u64 = 32;
// const UMCG_WORKER_ID_SHIFT: u64 = 5; // Define this based on your C code or intended logic
//
//
// // Events that can be received from workers
// #[derive(Debug, Clone, Copy)]
// enum WorkerEvent {
//     Block = 1,
//     Wake = 2,
//     Wait = 3,
//     Exit = 4,
//     Timeout = 5,
//     Preempt = 6,
// }
//
// #[derive(Debug)]
// enum WorkerState {
//     Idle,
//     Running,
//     Blocked,
// }
//
// // Represents a UMCG worker thread
// struct Worker {
//     id: u64,
//     state: WorkerState,
// }
//
// impl Worker {
//     // fn new(id: u64) -> io::Result<Self> {
//     //     // Register this as a UMCG worker
//     //     unsafe {
//     //         umcg_syscall(
//     //             0,
//     //             UmcgCmd::RegisterWorker as u64,
//     //             0,
//     //             id,
//     //             std::ptr::null_mut(),
//     //             0,
//     //         )?;
//     //     }
//     //
//     //     Ok(Worker {
//     //         id,
//     //         state: WorkerState::Idle,
//     //     })
//     // }
//     fn new(id: u64) -> io::Result<Self> {
//         println!("Beginning worker registration");
//
//         // Get current thread ID
//         let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
//         println!("Current gettid: {}", tid);
//
//         let worker_id = tid << UMCG_WORKER_ID_SHIFT;
//         println!("Shifted worker ID: {}", worker_id);
//
//         // Match exactly the C test code registration
//         unsafe {
//             let result = umcg_syscall(
//                 0,                         // flags
//                 UmcgCmd::RegisterWorker as u64,
//                 0,                         // next_tid
//                 worker_id,                 // abs_timeout
//                 std::ptr::null_mut(),      // events
//                 0,                         // event_sz
//             );
//             match result {
//                 Ok(r) => {
//                     println!("Worker registration succeeded with result: {}", r);
//                     Ok(Worker {
//                         id: worker_id,
//                         state: WorkerState::Idle,
//                     })
//                 }
//                 Err(e) => {
//                     println!("Worker registration failed with error: {:?}, tid: {}, worker_id: {}",
//                              e, tid, worker_id);
//                     Err(e)
//                 }
//             }
//         }
//     }
// }
//
// impl Drop for Worker {
//     fn drop(&mut self) {
//         unsafe {
//             let _ = umcg_syscall(
//                 0,
//                 UmcgCmd::Unregister as u64,
//                 0,
//                 0,
//                 std::ptr::null_mut(),
//                 0,
//             );
//         }
//     }
// }
//
// // Type alias for tasks
// type Task = Box<dyn FnOnce() + Send + 'static>;
//
// // Server running on a dedicated kernel thread
// struct Server {
//     id: usize,
//     task_receiver: Receiver<Task>,
//     idle_workers: VecDeque<Worker>,
//     active_workers: HashMap<u64, Worker>,
//     next_worker_id: AtomicU64,
//     local_data: HashMap<String, Vec<u8>>, // Example of server-local state that doesn't need synchronization
// }
//
// impl Server {
//     fn new(id: usize, task_receiver: Receiver<Task>) -> io::Result<Self> {
//         // Register as a UMCG server
//         unsafe {
//             umcg_syscall(
//                 0,
//                 UmcgCmd::RegisterServer as u64,
//                 0,
//                 0,
//                 std::ptr::null_mut(),
//                 0,
//             )?;
//         }
//
//         Ok(Server {
//             id,
//             task_receiver,
//             idle_workers: VecDeque::new(),
//             active_workers: HashMap::new(),
//             next_worker_id: AtomicU64::new(0),
//             local_data: HashMap::new(),
//         })
//     }
//
//     fn generate_worker_id(&self) -> u64 {
//         self.next_worker_id.fetch_add(UMCG_WORKER_ID_ALIGNMENT, Ordering::SeqCst)
//     }
//
//     fn get_worker(&mut self) -> io::Result<Worker> {
//         if let Some(worker) = self.idle_workers.pop_front() {
//             return Ok(worker);
//         }
//
//         // Create new worker if pool is empty
//         let worker_id = self.generate_worker_id();
//         Worker::new(worker_id)
//     }
//
//     // fn run(&mut self) -> io::Result<()> {
//     //     let mut events = [0u64; 64];
//     //
//     //     loop {
//     //         // Process any pending tasks
//     //         while let Ok(task) = self.task_receiver.try_recv() {
//     //             self.execute_task(task)?;
//     //         }
//     //
//     //         // Wait for worker events
//     //         unsafe {
//     //             umcg_syscall(
//     //                 0,
//     //                 UmcgCmd::Wait as u64,
//     //                 0,
//     //                 0,
//     //                 events.as_mut_ptr(),
//     //                 events.len() as i32,
//     //             )?;
//     //         }
//     //
//     //         // Handle events
//     //         for &event in &events {
//     //             if event == 0 {
//     //                 break;
//     //             }
//     //             self.handle_worker_event(event)?;
//     //         }
//     //     }
//     // }
//     fn run(&mut self) -> io::Result<()> {
//         println!("Server {} starting run loop", self.id);
//         let mut events = [0u64; 64];
//
//         // Create a short timeout (1ms) for the wait syscall
//         use std::time::{SystemTime, UNIX_EPOCH};
//         let short_timeout = || {
//             SystemTime::now()
//                 .duration_since(UNIX_EPOCH)
//                 .unwrap()
//                 .as_nanos() as u64
//                 + 1_000_000  // 1ms in nanoseconds
//         };
//
//         loop {
//             println!("Server {} checking for tasks", self.id);
//             // Process any pending tasks
//             while let Ok(task) = self.task_receiver.try_recv() {
//                 println!("Server {} received task", self.id);
//                 self.execute_task(task)?;
//             }
//
//             println!("Server {} waiting for events", self.id);
//             // Wait for worker events with timeout
//             unsafe {
//                 println!("Server {} entering umcg wait with timeout", self.id);
//                 let timeout = short_timeout();
//                 let wait_result = umcg_syscall(
//                     0,
//                     UmcgCmd::Wait as u64,
//                     0,
//                     timeout,
//                     events.as_mut_ptr(),
//                     events.len() as i32,
//                 );
//                 match wait_result {
//                     Ok(r) => println!("Server {} wait returned: {}", self.id, r),
//                     Err(e) => {
//                         println!("Server {} wait failed: {:?}", self.id, e);
//                         // Don't return error on timeout
//                         if e.kind() != io::ErrorKind::TimedOut {
//                             return Err(e);
//                         }
//                     }
//                 }
//             }
//
//             // Handle events
//             for &event in &events {
//                 if event == 0 {
//                     break;
//                 }
//                 println!("Server {} handling event: {}", self.id, event);
//                 self.handle_worker_event(event)?;
//             }
//         }
//     }
//
//     // fn execute_task(&mut self, task: Task) -> io::Result<()> {
//     //     // Get an available worker
//     //     let worker = self.get_worker()?;
//     //     let worker_id = worker.id;
//     //
//     //     // Add to active workers before executing
//     //     self.active_workers.insert(worker_id, worker);
//     //
//     //     // To execute on the worker, we need to context switch to it
//     //     unsafe {
//     //         // CTX_SWITCH command tells UMCG to switch from server to worker
//     //         umcg_syscall(
//     //             0,
//     //             UmcgCmd::CtxSwitch as u64,
//     //             worker_id as i32,  // worker to switch to
//     //             0,                 // no timeout
//     //             std::ptr::null_mut(),
//     //             0,
//     //         )?;
//     //     }
//     //
//     //     // Now we're running in the worker context
//     //     task();
//     //
//     //     // Switch back to server context
//     //     unsafe {
//     //         umcg_syscall(
//     //             0,
//     //             UmcgCmd::Wait as u64,  // Worker yields back to server
//     //             0,
//     //             0,
//     //             std::ptr::null_mut(),
//     //             0,
//     //         )?;
//     //     }
//     //
//     //     // Now back in server context, return worker to idle pool
//     //     if let Some(mut worker) = self.active_workers.remove(&worker_id) {
//     //         worker.state = WorkerState::Idle;
//     //         self.idle_workers.push_back(worker);
//     //     }
//     //
//     //     Ok(())
//     // }
//
//     fn execute_task(&mut self, task: Task) -> io::Result<()> {
//         println!("Server executing task");
//         let worker = self.get_worker()?;
//         let worker_id = worker.id;
//         println!("Got worker with id: {}", worker_id);
//
//         self.active_workers.insert(worker_id, worker);
//
//         println!("Context switching to worker");
//         unsafe {
//             let switch_result = umcg_syscall(
//                 0,
//                 UmcgCmd::CtxSwitch as u64,
//                 worker_id as i32,
//                 0,
//                 std::ptr::null_mut(),
//                 0,
//             );
//             match switch_result {
//                 Ok(r) => println!("Context switch succeeded with result: {}", r),
//                 Err(e) => {
//                     println!("Context switch failed with error: {:?}", e);
//                     return Err(e);
//                 }
//             }
//         }
//
//         println!("Running task in worker context");
//         task();
//
//         println!("Switching back to server context");
//         unsafe {
//             let wait_result = umcg_syscall(
//                 0,
//                 UmcgCmd::Wait as u64,
//                 0,
//                 0,
//                 std::ptr::null_mut(),
//                 0,
//             );
//             match wait_result {
//                 Ok(r) => println!("Switch back succeeded with result: {}", r),
//                 Err(e) => {
//                     println!("Switch back failed with error: {:?}", e);
//                     return Err(e);
//                 }
//             }
//         }
//
//         if let Some(mut worker) = self.active_workers.remove(&worker_id) {
//             worker.state = WorkerState::Idle;
//             self.idle_workers.push_back(worker);
//         }
//         println!("Task execution complete");
//
//         Ok(())
//     }
//
//     fn handle_worker_event(&mut self, event: u64) -> io::Result<()> {
//         let worker_id = event & !(UMCG_WORKER_ID_ALIGNMENT - 1);
//         let event_type = event & (UMCG_WORKER_ID_ALIGNMENT - 1);
//
//         if let Some(worker) = self.active_workers.get_mut(&worker_id) {
//             match event_type {
//                 1 => worker.state = WorkerState::Blocked,  // Block
//                 2 => worker.state = WorkerState::Running,  // Wake
//                 3 => {  // Wait
//                     worker.state = WorkerState::Blocked;
//                     // Could implement special handling for waiting workers
//                 }
//                 4 => {  // Exit
//                     // Remove worker from active set
//                     self.active_workers.remove(&worker_id);
//                 }
//                 5 => {  // Timeout
//                     // Handle timeout - maybe reschedule the task
//                 }
//                 6 => {  // Preempt
//                     // Handle preemption - maybe move to end of queue
//                 }
//                 _ => return Err(io::Error::new(io::ErrorKind::Other, "Unknown event")),
//             }
//         }
//
//         Ok(())
//     }
// }
//
// impl Drop for Server {
//     fn drop(&mut self) {
//         unsafe {
//             let _ = umcg_syscall(
//                 0,
//                 UmcgCmd::Unregister as u64,
//                 0,
//                 0,
//                 std::ptr::null_mut(),
//                 0,
//             );
//         }
//     }
// }
//
// // The public executor interface
// pub struct ShardedExecutor {
//     task_senders: Vec<Sender<Task>>,
//     num_servers: usize,
// }
//
// impl ShardedExecutor {
//     pub fn new(num_servers: usize) -> io::Result<Self> {
//         let mut task_senders = Vec::with_capacity(num_servers);
//
//         // Create a server per core
//         for id in 0..num_servers {
//             let (sender, receiver) = channel();
//             task_senders.push(sender);
//
//             // Spawn the server on a dedicated kernel thread
//             std::thread::spawn(move || {
//                 let mut server = Server::new(id, receiver).unwrap();
//                 server.run().unwrap();
//             });
//         }
//
//         Ok(ShardedExecutor {
//             task_senders,
//             num_servers,
//         })
//     }
//
//     // pub fn spawn<F>(&self, shard_key: usize, f: F)
//     // where
//     //     F: FnOnce() + Send + 'static,
//     // {
//     //     let server_id = shard_key % self.num_servers;
//     //     let task = Box::new(f);
//     //
//     //     self.task_senders[server_id].send(task)
//     //         .expect("Failed to send task to server");
//     // }
//
//     pub fn spawn<F>(&self, shard_key: usize, f: F)
//     where
//         F: FnOnce() + Send + 'static,
//     {
//         println!("ShardedExecutor spawn called with shard_key: {}", shard_key);
//         let server_id = shard_key % self.num_servers;
//         println!("Selected server_id: {}", server_id);
//         let task = Box::new(f);
//         println!("Task boxed, attempting to send to server");
//
//         match self.task_senders[server_id].send(task) {
//             Ok(_) => println!("Task sent successfully to server {}", server_id),
//             Err(e) => println!("Failed to send task to server {}: {:?}", server_id, e),
//         }
//         println!("Spawn complete");
//     }
// }
//
// // Syscall wrapper for aarch64
// #[cfg(target_arch = "aarch64")]
// unsafe fn umcg_syscall(
//     flags: u64,
//     cmd: u64,
//     next_tid: i32,
//     abs_timeout: u64,
//     events: *mut u64,
//     event_sz: i32,
// ) -> io::Result<i64> {
//     let ret: i64;
//     std::arch::asm!(
//     "svc 0",
//     in("x8") SYS_UMCG,    // Syscall number goes in x8
//     in("x0") flags,       // Arguments go in x0-x5
//     in("x1") cmd,
//     in("x2") next_tid,
//     in("x3") abs_timeout,
//     in("x4") events,
//     in("x5") event_sz,
//     lateout("x0") ret,    // Return value comes in x0
//     );
//
//     if ret < 0 {
//         Err(io::Error::from_raw_os_error(-ret as i32))
//     } else {
//         Ok(ret)
//     }
// }
//
// // Keep x86_64 version for completeness
// #[cfg(target_arch = "x86_64")]
// unsafe fn umcg_syscall(
//     flags: u64,
//     cmd: u64,
//     next_tid: i32,
//     abs_timeout: u64,
//     events: *mut u64,
//     event_sz: i32,
// ) -> io::Result<i64> {
//     println!("UMCG syscall - cmd: {}, flags: {}, tid: {}, timeout: {}", cmd, flags, next_tid, abs_timeout);
//     let ret: i64;
//     std::arch::asm!(
//     "syscall",
//     in("rax") SYS_UMCG,
//     in("rdi") flags,
//     in("rsi") cmd,
//     in("rdx") next_tid,
//     in("r10") abs_timeout,
//     in("r8") events,
//     in("r9") event_sz,
//     out("rcx") _,
//     out("r11") _,
//     lateout("rax") ret,
//     );
//
//     println!("UMCG syscall raw return: {}", ret);
//     if ret < 0 {
//         let err = io::Error::from_raw_os_error(-ret as i32);
//         println!("UMCG syscall error details: code={}, kind={:?}, msg={}",
//                  err.raw_os_error().unwrap_or(-1),
//                  err.kind(),
//                  err.to_string()
//         );
//         Err(err)
//     } else {
//         Ok(ret)
//     }
// }
//
// pub fn test_http_like_workloads() {
//     let num_cores = num_cpus::get();
//     let executor = ShardedExecutor::new(num_cores).unwrap();
//
//     // Simulate HTTP requests
//     for user_id in 0..100 {
//         let shard_key = user_id; // Shard by user_id
//         executor.spawn(shard_key, move || {
//             // Simulate request handling
//             std::thread::sleep(std::time::Duration::from_millis(10));
//         });
//     }
//
//     // Wait for tasks to complete
//     std::thread::sleep(std::time::Duration::from_secs(2));
// }
//
//
//
// // Example usage with an HTTP server
// #[cfg(test)]
// mod tests {
//     use super::*;
//     use std::time::Duration;
//
//     #[test]
//     fn test_http_like_workload() {
//         let num_cores = num_cpus::get();
//         let executor = ShardedExecutor::new(num_cores).unwrap();
//
//         // Simulate HTTP requests
//         for user_id in 0..100 {
//             let shard_key = user_id; // Shard by user_id
//             executor.spawn(shard_key, move || {
//                 // Simulate request handling
//                 std::thread::sleep(Duration::from_millis(10));
//             });
//         }
//
//         // Wait for tasks to complete
//         std::thread::sleep(Duration::from_secs(2));
//     }
// }