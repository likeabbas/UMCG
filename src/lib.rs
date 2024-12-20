use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::collections::{VecDeque, HashMap};
use libc::{syscall, SYS_gettid, pid_t};

const SYS_UMCG_CTL: i64 = 450;
const UMCG_WORKER_ID_SHIFT: u64 = 5;

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

fn log_with_timestamp(msg: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    println!("[{:>3}.{:03}] {}",
             now.as_secs() % 1000,
             now.subsec_millis(),
             msg);
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
        if ret != -1 || unsafe { *libc::__errno_location() } != libc::EINTR {
            return ret;
        }
        flags = 1; // UMCG_WAIT_FLAG_INTERRUPTED
    }
}

// Generic worker function type
type WorkerFn = Box<dyn Fn(usize) + Send + 'static>;

// Worker thread function
fn run_worker(id: usize, worker_fn: WorkerFn, workers: Arc<AtomicI32>, _done: Arc<AtomicBool>) {
    let tid = get_thread_id();
    log_with_timestamp(&format!("Worker {} (tid: {}) starting", id, tid));

    assert_eq!(
        sys_umcg_ctl(
            0,
            UmcgCmd::RegisterWorker,
            0,
            (tid as u64) << UMCG_WORKER_ID_SHIFT,
            None,
            0
        ),
        0
    );

    workers.fetch_add(1, Ordering::Relaxed);

    worker_fn(id);

    log_with_timestamp(&format!("Worker {}: Shutting down", id));
    workers.fetch_sub(1, Ordering::Relaxed);
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

pub fn run_umcg_workers<F>(worker_count: usize, worker_fn: F) -> i32
where
    F: Fn(usize) + Send + Clone + 'static
{
    log_with_timestamp(&format!("Starting UMCG demo with {} workers...", worker_count));

    // Initialize server
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    let workers_count = Arc::new(AtomicI32::new(0));
    let done = Arc::new(AtomicBool::new(false));

    // Spawn workers
    let mut handles = Vec::new();
    for i in 0..worker_count {
        let workers = workers_count.clone();
        let done_flag = done.clone();
        let worker_fn = Box::new(worker_fn.clone());

        handles.push(thread::spawn(move || {
            run_worker(i, worker_fn, workers, done_flag)
        }));
    }

    // Track runnable workers in a queue
    let mut runnable_workers = VecDeque::new();
    let mut completed_cycles = HashMap::new();

    // Server main loop
    let test_start = SystemTime::now();

    while !done.load(Ordering::Relaxed) || workers_count.load(Ordering::Relaxed) != 0 {
        let mut events = [0u64; 6]; // Space for multiple events
        let ret = if let Some(&next_worker) = runnable_workers.front() {
            log_with_timestamp(&format!("Server: Context switching to worker {}", next_worker));
            sys_umcg_ctl(
                0,
                UmcgCmd::CtxSwitch,
                next_worker,
                0,
                Some(&mut events),
                6
            )
        } else {
            log_with_timestamp("Server: Waiting for worker events...");
            umcg_wait_retry(0, Some(&mut events), 6)
        };

        if ret != 0 {
            eprintln!("Server loop error");
            return -1;
        }

        // Process all non-zero events
        for event in events.iter().take_while(|&&e| e != 0) {
            let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
            let worker_tid = event >> UMCG_WORKER_ID_SHIFT;

            match event_type {
                1 => {  // BLOCK
                    log_with_timestamp(&format!("Server: Worker {} blocked", worker_tid));
                    if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                        runnable_workers.remove(pos);
                    }
                },
                2 => {  // WAKE
                    log_with_timestamp(&format!("Server: Worker {} woke up", worker_tid));
                    if !runnable_workers.contains(&(worker_tid as i32)) {
                        runnable_workers.push_back(worker_tid as i32);
                    }
                },
                3 => {  // WAIT
                    log_with_timestamp(&format!("Server: Worker {} yielded", worker_tid));
                    if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                        runnable_workers.remove(pos);
                        runnable_workers.push_back(worker_tid as i32);
                    }
                },
                4 => {  // EXIT
                    log_with_timestamp(&format!("Server: Worker {} exited", worker_tid));
                    if let Some(pos) = runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                        runnable_workers.remove(pos);
                    }
                    completed_cycles.insert(worker_tid, true);

                    if completed_cycles.len() == worker_count {
                        done.store(true, Ordering::Relaxed);
                    }
                },
                _ => log_with_timestamp(&format!("Server: Unknown event {} from worker {}", event_type, worker_tid)),
            }

            // Implement round-robin scheduling
            if runnable_workers.front() == Some(&(worker_tid as i32)) {
                if let Some(worker) = runnable_workers.pop_front() {
                    runnable_workers.push_back(worker);
                }
            }

            if event_type == 4 && completed_cycles.len() == worker_count {
                let test_duration = SystemTime::now().duration_since(test_start).unwrap();
                log_with_timestamp(&format!(
                    "Test completed in {}.{:03} seconds",
                    test_duration.as_secs(),
                    test_duration.subsec_millis()
                ));
                break;
            }
        }
    }

    // Clean up
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    log_with_timestamp("UMCG workers demo completed");
    0
}

// Example usage - recreate our original test with 3 workers
pub fn run_original_demo() -> i32 {
    run_umcg_workers(3, |id| {
        // Do exactly two sleep cycles
        for _ in 0..2 {
            let sleep_duration = match id {
                0 => 2, // Worker A
                1 => 3, // Worker B
                _ => 4, // Worker C
            };

            log_with_timestamp(&format!("Worker {}: Starting first sleep", id));
            thread::sleep(Duration::from_secs(sleep_duration));

            log_with_timestamp(&format!("Worker {}: Between sleeps", id));
            thread::sleep(Duration::from_secs(sleep_duration));

            log_with_timestamp(&format!("Worker {}: Finished both sleeps", id));
        }
    })
}

