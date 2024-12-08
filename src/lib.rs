use libc::{self, pid_t, syscall, SYS_gettid};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::collections::VecDeque;

const SYS_UMCG_CTL: i64 = 450;
const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;

fn log(thread: &str, msg: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0));
    println!("[{:>10}] [{:>12}] {}",
             format!("{:03}.{:03}", now.as_secs() % 1000, now.subsec_millis()),
             thread,
             msg
    );
}

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

impl UmcgEventType {
    fn from_u64(value: u64) -> Option<Self> {
        match value {
            1 => Some(UmcgEventType::Block),
            2 => Some(UmcgEventType::Wake),
            3 => Some(UmcgEventType::Wait),
            4 => Some(UmcgEventType::Exit),
            5 => Some(UmcgEventType::Timeout),
            6 => Some(UmcgEventType::Preempt),
            _ => None,
        }
    }
}

// Represents an I/O task
#[derive(Debug)]
struct Task {
    id: usize,
    operation: String,
    duration: Duration,
    completed: bool,
}

// Shared state between server and worker
struct SharedState {
    current_task: Option<Task>,
    done: AtomicBool,
}

fn worker_thread(state: Arc<parking_lot::RwLock<SharedState>>) -> pid_t {
    let tid = unsafe { syscall(SYS_gettid) } as pid_t;
    log("worker", &format!("Started with TID: {}", tid));

    // Register as a worker
    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::RegisterWorker,
        0,
        (tid as u64) << UMCG_WORKER_ID_SHIFT,
        None,
        0,
    );
    assert_eq!(ret, 0, "Failed to register worker");

    // Initial signal that we're ready
    log("worker", "Ready for tasks");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 0), 0);

    while !state.read().done.load(Ordering::Relaxed) {
        let guard = state.read();
        if let Some(task) = &guard.current_task {
            // Simulate blocking I/O with sleep
            log("worker", &format!("Starting {} operation for task {}", task.operation, task.id));
            drop(guard);  // Drop the lock before sleeping

            // Sleep will cause the kernel to block this thread and notify the server
            thread::sleep(task.duration);

            log("worker", &format!("Completed {} operation for task {}", task.operation, task.id));
        }
    }

    log("worker", "Exiting");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    tid
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

pub fn run_scheduler_example() -> i32 {
    // Register as server
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
    log("server", "Registered");

    // Create our task queue with simulated I/O operations
    let mut ready_queue: VecDeque<Task> = vec![
        Task {
            id: 1,
            operation: "read".into(),
            duration: Duration::from_secs(2),
            completed: false
        },
        Task {
            id: 2,
            operation: "write".into(),
            duration: Duration::from_secs(1),
            completed: false
        },
    ].into();

    let state = Arc::new(parking_lot::RwLock::new(SharedState {
        current_task: None,
        done: AtomicBool::new(false),
    }));

    // Create worker thread
    log("server", "Creating worker thread");
    let worker_handle = thread::spawn({
        let state = state.clone();
        move || worker_thread(state)
    });

    // Wait for worker registration
    let mut events = [0u64; 2];
    log("server", "Waiting for worker registration");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, Some(&mut events), 2), 0);

    let worker_id = events[0] & !UMCG_WORKER_EVENT_MASK;
    let worker_tid = (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t;
    let event_type = UmcgEventType::from_u64(events[0] & UMCG_WORKER_EVENT_MASK)
        .expect("Invalid event type received");
    log("server", &format!("Worker registered - ID: {}, Event: {:?}", worker_id, event_type));

    // Main scheduling loop
    log("server", "Starting scheduling loop");

    while !ready_queue.is_empty() {
        // Schedule next task if worker is free
        if state.read().current_task.is_none() {
            if let Some(task) = ready_queue.pop_front() {
                log("server", &format!("Scheduling task {} ({} operation)", task.id, task.operation));
                state.write().current_task = Some(task);

                // Context switch to worker
                let mut events = [0u64; 2];
                assert_eq!(
                    sys_umcg_ctl(0, UmcgCmd::CtxSwitch, worker_tid, 0, Some(&mut events), 2),
                    0
                );

                let event_type = UmcgEventType::from_u64(events[0] & UMCG_WORKER_EVENT_MASK)
                    .expect("Invalid event type received");
                log("server", &format!("Context switch result: {:?}", event_type));
            }
        }

        // Wait for worker events
        let mut events = [0u64; 2];
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, Some(&mut events), 2), 0);

        let event_type = UmcgEventType::from_u64(events[0] & UMCG_WORKER_EVENT_MASK)
            .expect("Invalid event type received");
        log("server", &format!("Received event: {:?}", event_type));

        match event_type {
            UmcgEventType::Block => {
                log("server", "Worker blocked on I/O operation");
                // Just wait for the Wake event, worker is blocked
            },
            UmcgEventType::Wake => {
                if let Some(task) = state.write().current_task.take() {
                    log("server", &format!("Task {} I/O operation completed", task.id));
                }
            },
            evt => log("server", &format!("Unexpected event: {:?}", evt)),
        }
    }

    log("server", "All tasks complete");

    // Clean up
    state.write().done.store(true, Ordering::Relaxed);
    let worker_tid = worker_handle.join().unwrap();
    log("server", &format!("Worker thread {} joined", worker_tid));

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    log("server", "Unregistered");

    0
}

fn main() {
    log("main", "Starting UMCG scheduler example");
    std::process::exit(run_scheduler_example());
}