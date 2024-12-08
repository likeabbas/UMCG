use libc::{self, c_int, pid_t, syscall, SYS_gettid, CLOCK_REALTIME, timespec, EINTR, EINVAL, ETIMEDOUT, ESRCH, EFAULT, EAGAIN};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::ptr;
use std::io::Write;
use UMCG::{run_multiplex_test};
// Add this line for flush()

const SYS_UMCG_CTL: i64 = 450;
const SYS_FUTEX: i64 = 202;

const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;
const BILLION: u64 = 1_000_000_000;

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_PRIVATE_FLAG: i32 = 128;

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

static DONE: AtomicBool = AtomicBool::new(false);

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

fn get_thread_id() -> pid_t {
    unsafe { syscall(SYS_gettid) as pid_t }
}

fn umcg_wait_retry(worker_id: u64, mut events_buf: Option<&mut [u64]>, event_sz: i32) -> i32 {
    let mut flags = 0;
    loop {
        // Create a new Option that reborrrows the slice for each iteration
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
        flags = UMCG_WAIT_FLAG_INTERRUPTED;
    }
}

fn umcg_yield() {
    assert_eq!(umcg_wait_retry(0, None, 0), 0);
}

fn umcg_wait_any_worker(expected_event: UmcgEventType) -> u64 {
    let mut events = [0u64; 2];
    assert_eq!(umcg_wait_retry(0, Some(&mut events), 2), 0);
    assert_eq!(events[0] & UMCG_WORKER_EVENT_MASK, expected_event as u64);
    assert_eq!(events[1], 0);
    events[0] & !UMCG_WORKER_EVENT_MASK
}

fn umcg_assert_worker_event(event: u64, worker_id: u64, event_type: UmcgEventType) {
    assert_eq!(event & UMCG_WORKER_EVENT_MASK, event_type as u64);
    assert_eq!(event & !UMCG_WORKER_EVENT_MASK, worker_id);
}

fn umcg_ctxsw_assert_worker_event(worker_id: u64, event_type: UmcgEventType) {
    let mut events = [0u64; 2];
    assert_eq!(
        sys_umcg_ctl(
            0,
            UmcgCmd::CtxSwitch,
            (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            Some(&mut events),
            2
        ),
        0
    );
    umcg_assert_worker_event(events[0], worker_id, event_type);
    assert_eq!(events[1], 0);
}

struct WorkerThreads {
    count: Arc<AtomicI32>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkerThreads {
    fn new() -> Self {
        WorkerThreads {
            count: Arc::new(AtomicI32::new(0)),
            handles: Vec::new(),
        }
    }

    fn spawn_worker<F>(&mut self, f: F)
    where
        F: FnOnce(Arc<AtomicI32>) + Send + 'static,
    {
        let count = self.count.clone();
        self.handles.push(thread::spawn(move || f(count)));
    }

    fn join_all(self) {
        for handle in self.handles {
            handle.join().unwrap();
        }
    }
}

fn demo_worker_a(workers: Arc<AtomicI32>) {
    let tid = get_thread_id();
    println!("A == {}", tid);

    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::RegisterWorker,
        0,
        ((tid as u64) << UMCG_WORKER_ID_SHIFT),
        None,
        0,
    );
    assert_eq!(ret, 0);

    workers.fetch_add(1, Ordering::Relaxed);
    let mut i = 0u64;
    while !DONE.load(Ordering::Relaxed) {
        if i % 1_000_000 == 0 {
            print!(".");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
        }
        if i % 10_000_000 == 0 {
            umcg_yield();
        }
        i += 1;
    }

    println!("A == done");
    workers.fetch_sub(1, Ordering::Relaxed);
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

fn demo_worker_b(workers: Arc<AtomicI32>) {
    let tid = get_thread_id();
    println!("B == {}", tid);

    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::RegisterWorker,
        0,
        ((tid as u64) << UMCG_WORKER_ID_SHIFT),
        None,
        0,
    );
    assert_eq!(ret, 0);

    workers.fetch_add(1, Ordering::Relaxed);
    while !DONE.load(Ordering::Relaxed) {
        println!("B");
        thread::sleep(Duration::from_secs(1));
    }

    println!("B == done");
    workers.fetch_sub(1, Ordering::Relaxed);
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

fn demo_worker_c(workers: Arc<AtomicI32>) {
    let tid = get_thread_id();
    println!("C == {}", tid);

    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::RegisterWorker,
        0,
        ((tid as u64) << UMCG_WORKER_ID_SHIFT),
        None,
        0,
    );
    assert_eq!(ret, 0);

    workers.fetch_add(1, Ordering::Relaxed);
    while !DONE.load(Ordering::Relaxed) {
        println!("C");
        thread::sleep(Duration::from_secs(2));
    }

    println!("C == done");
    workers.fetch_sub(1, Ordering::Relaxed);
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

fn run_demo_test() -> i32 {
    let mut workers = WorkerThreads::new();

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    workers.spawn_worker(demo_worker_a);
    workers.spawn_worker(demo_worker_b);
    workers.spawn_worker(demo_worker_c);

    let start = SystemTime::now();
    let test_duration = Duration::from_secs(10);

    while !DONE.load(Ordering::Relaxed) || workers.count.load(Ordering::Relaxed) != 0 {
        let mut events = vec![0u64; 6];
        let ret = umcg_wait_retry(0, Some(&mut events), 6);

        if ret != 0 {
            eprintln!("Server loop error");
            return -1;
        }

        for event in events.iter().take_while(|&&e| e != 0) {
            let event_type = event & UMCG_WORKER_EVENT_MASK;
            match event_type {
                2 | 3 => { // WAKE or WAIT events
                    println!("Worker event: {}", event_type);
                }
                _ => {
                    println!(
                        "Worker tid {}, event {}",
                        event >> UMCG_WORKER_ID_SHIFT,
                        event & UMCG_WORKER_EVENT_MASK
                    );
                }
            }
        }

        if start.elapsed().unwrap() >= test_duration {
            DONE.store(true, Ordering::Relaxed);
        }
    }

    workers.join_all();
    0
}

fn run_basic_tests() {
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    // Test worker registration with invalid ID
    let ret = sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, UMCG_WORKER_EVENT_MASK, None, 0);
    assert!(ret < 0);
    assert_eq!(unsafe { *libc::__errno_location() }, libc::EINVAL);

    // Test context switch to self
    let mut events = [0u64; 2];
    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::CtxSwitch,
        get_thread_id(),
        0,
        Some(&mut events),
        2,
    );
    assert_eq!(ret, -1);
    assert_eq!(unsafe { *libc::__errno_location() }, libc::EINVAL);

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    println!("Basic tests passed!");
}

fn futex_perftest() -> i32 {
    let done = Arc::new(AtomicBool::new(false));
    let futex_word = Arc::new(AtomicI32::new(0));
    let done_clone = done.clone();
    let futex_clone = futex_word.clone();

    println!("Futex performance test duration: 10 seconds...");

    let worker = thread::spawn(move || {
        while !done_clone.load(Ordering::Relaxed) {
            unsafe {
                syscall(
                    SYS_FUTEX,
                    &futex_clone as *const _ as i64,
                    FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                    0,
                    0,
                    ptr::null::<i32>(),
                    0,
                );
            }
            futex_clone.store(0, Ordering::SeqCst);
            unsafe {
                syscall(
                    SYS_FUTEX,
                    &futex_clone as *const _ as i64,
                    FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
                    1,
                    0,
                    ptr::null::<i32>(),
                    0,
                );
            }
        }
    });

    let start = SystemTime::now();
    let mut cycles = 0i64;

    while !done.load(Ordering::Relaxed) {
        futex_word.store(1, Ordering::SeqCst);
        unsafe {
            syscall(
                SYS_FUTEX,
                &futex_word as *const _ as i64,
                FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
                1,
                0,
                ptr::null::<i32>(),
                0,
            );
            syscall(
                SYS_FUTEX,
                &futex_word as *const _ as i64,
                FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                1,
                0,
                ptr::null::<i32>(),
                0,
            );
        }
        cycles += 1;

        if start.elapsed().unwrap() >= Duration::from_secs(10) {
            done.store(true, Ordering::Relaxed);
        }
    }

    println!("Results: {} cycles ({} cycles/sec)", cycles, cycles as f64 / 10.0);
    futex_word.store(1, Ordering::SeqCst);
    unsafe {
        syscall(
            SYS_FUTEX,
            &futex_word as *const _ as i64,
            FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
            1,
            0,
            ptr::null::<i32>(),
            0,
        );
    }
    worker.join().unwrap();
    0
}

fn umcg_test_basic() {
    let mut worker_ids = [0u64; 2];

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    // Test 1: Blocking worker
    let handle = thread::spawn(|| {
        let tid = get_thread_id();
        assert_eq!(
            sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
            0
        );
        thread::sleep(Duration::from_micros(16 * 1024));
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    });

    worker_ids[0] = umcg_wait_any_worker(UmcgEventType::Wake);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Block);
    assert_eq!(umcg_wait_retry(worker_ids[0], None, 0), 0);  // wait for worker to unblock
    assert_eq!(umcg_wait_any_worker(UmcgEventType::Wake), worker_ids[0]);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Exit);
    handle.join().unwrap();

    // Test 2: Wait timeout worker
    let handle = thread::spawn(|| {
        let tid = get_thread_id();
        assert_eq!(
            sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
            0
        );
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(
            sys_umcg_ctl(0, UmcgCmd::Wait, 0, now.as_nanos() as u64, None, 0),
            -1
        );
        assert_eq!(unsafe { *libc::__errno_location() }, ETIMEDOUT);
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    });

    worker_ids[0] = umcg_wait_any_worker(UmcgEventType::Wake);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Wait);
    assert_eq!(umcg_wait_any_worker(UmcgEventType::Timeout), worker_ids[0]);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Exit);
    handle.join().unwrap();

    // Test 3: Dummy worker + Context switch timeout worker
    let (worker1, worker2) = {
        let handle1 = thread::spawn(|| {
            let tid = get_thread_id();
            assert_eq!(
                sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
                0
            );
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
        });

        worker_ids[0] = umcg_wait_any_worker(UmcgEventType::Wake);
        let worker_id = worker_ids[0];

        let handle2 = thread::spawn(move || {
            let tid = get_thread_id();
            assert_eq!(
                sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
                0
            );
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            let ret = sys_umcg_ctl(
                0,
                UmcgCmd::CtxSwitch,
                (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
                now.as_nanos() as u64,
                None,
                0
            );
            assert!((ret == 0) || ((ret < 0) && unsafe { *libc::__errno_location() } == ETIMEDOUT));
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
        });

        (handle1, handle2)
    };

    worker_ids[1] = umcg_wait_any_worker(UmcgEventType::Wake);
    let mut events = [0u64; 2];

    assert_eq!(
        sys_umcg_ctl(
            0,
            UmcgCmd::CtxSwitch,
            (worker_ids[1] >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            Some(&mut events),
            2
        ),
        0
    );
    umcg_assert_worker_event(events[0], worker_ids[0], UmcgEventType::Exit);
    assert_eq!(events[1], 0);

    assert_eq!(umcg_wait_retry(0, Some(&mut events), 2), 0);
    umcg_assert_worker_event(events[0], worker_ids[1], UmcgEventType::Wait);
    if events[1] != 0 {
        umcg_assert_worker_event(events[1], worker_ids[1], UmcgEventType::Timeout);
    }
    umcg_ctxsw_assert_worker_event(worker_ids[1], UmcgEventType::Exit);
    worker1.join().unwrap();
    worker2.join().unwrap();

    // Test 4: Preempted worker
    let worker_exit = Arc::new(AtomicBool::new(false));
    let worker_exit_clone = worker_exit.clone();
    let handle = thread::spawn(move || {
        let tid = get_thread_id();
        assert_eq!(
            sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
            0
        );
        while !worker_exit_clone.load(Ordering::Relaxed) {}
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    });

    worker_ids[0] = umcg_wait_any_worker(UmcgEventType::Wake);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Preempt);
    worker_exit.store(true, Ordering::Relaxed);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Exit);
    handle.join().unwrap();

    // Test 5: Exiting worker
    let handle = thread::spawn(|| {
        let tid = get_thread_id();
        let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;

        assert_eq!(
            sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, worker_id, None, 0),
            0
        );

        // Try to register an already registered worker
        let ret = sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, worker_id, None, 0);
        assert!(ret < 0 && unsafe { *libc::__errno_location() } == EINVAL);

        // Try to context-switch to itself
        let ret = sys_umcg_ctl(
            0,
            UmcgCmd::CtxSwitch,
            (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            None,
            0
        );
        assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);

        // Test various invalid parameter combinations
        let mut events = [0u64; 2];

        let ret = sys_umcg_ctl(0, UmcgCmd::Wait, (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t, 0, None, 0);
        assert!(ret < 0 && unsafe { *libc::__errno_location() } == EINVAL);

        let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, Some(&mut events), 0);
        assert!(ret < 0 && unsafe { *libc::__errno_location() } == EINVAL);

        let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 2);
        assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);
    });

    worker_ids[0] = umcg_wait_any_worker(UmcgEventType::Wake);
    umcg_ctxsw_assert_worker_event(worker_ids[0], UmcgEventType::Exit);
    handle.join().unwrap();

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

fn umcg_test_server2server() {
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    let handle = thread::spawn(|| {
        let mut events = [0u64; 2];
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

        // Wait for non-existing workers, until woken up by the main thread
        assert_eq!(umcg_wait_retry(0, Some(&mut events), 2), 0);
        assert_eq!(events[0], 0);

        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    });

    // Wake up the idle server that is waiting for non-existing workers
    loop {
        let ret = sys_umcg_ctl(0, UmcgCmd::Wake, 0, 0, None, 0);
        if ret == 0 {
            break;
        }
        assert!(ret < 0 && unsafe { *libc::__errno_location() } == EAGAIN);
    }

    handle.join().unwrap();
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
}

fn umcg_test_errors() {
    println!("Starting error tests...");

    // Test invalid worker registration
    let ret = sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, UMCG_WORKER_EVENT_MASK, None, 0);
    assert!(ret < 0 && unsafe { *libc::__errno_location() } == EINVAL);
    println!("Invalid registration test passed");

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
    println!("Server registered");

    // Try to context-switch to self
    let mut events = [0u64; 2];
    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::CtxSwitch,
        get_thread_id(),
        0,
        Some(&mut events),
        2
    );
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);
    println!("Self context-switch test passed");

    // Test wait with invalid event buffer size
    let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, Some(&mut events), 1);
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);
    println!("Invalid buffer size test passed");

    // Test wait with timeout (use a very short timeout)
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let ret = sys_umcg_ctl(
        0,
        UmcgCmd::Wait,
        0,
        now.as_nanos() as u64,
        Some(&mut events),
        2
    );
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == ETIMEDOUT);
    println!("Timeout test passed");

    // Test poll without blocking
    let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 0, 1, Some(&mut events), 2);
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == ETIMEDOUT);
    println!("Poll test passed");

    // Instead of using a dummy worker, test directly
    println!("Testing invalid parameter combinations...");

    // Test invalid waits
    let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 1, 0, Some(&mut events), 0);
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);
    println!("Invalid wait params test passed");

    let ret = sys_umcg_ctl(0, UmcgCmd::Wait, 1, 0, None, 2);
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == EINVAL);
    println!("Invalid wait params test 2 passed");

    // Test invalid context switch - non-existent thread
    let ret = sys_umcg_ctl(0, UmcgCmd::CtxSwitch, 1, 1, Some(&mut events), 2);
    println!("CtxSwitch test returned: {} with errno: {}", ret, unsafe { *libc::__errno_location() });
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == ESRCH);
    println!("Invalid ctx switch test passed");

    // Try to context-switch to another non-existing thread
    let ret = sys_umcg_ctl(0, UmcgCmd::CtxSwitch, 999999, 0, Some(&mut events), 2);
    println!("Non-existing thread switch returned: {} with errno: {}", ret, unsafe { *libc::__errno_location() });
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == ESRCH);
    println!("Non-existing thread test passed");

    // Try to wake up an idle server when there aren't any
    let ret = sys_umcg_ctl(0, UmcgCmd::Wake, 0, 0, None, 0);
    assert!(ret == -1 && unsafe { *libc::__errno_location() } == EAGAIN);
    println!("Wake idle server test passed");

    assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    println!("Error tests completed");
}

fn run_full_tests() {

    println!("Running others");
    // run_examples().expect("TODO: panic message");
    run_multiplex_test();

    println!("Running UMCG test suite...");

    println!("\nRunning basic tests...");
    umcg_test_basic();

    println!("\nRunning server-to-server tests...");
    umcg_test_server2server();

    println!("\nRunning error condition tests...");
    umcg_test_errors();

    println!("\nAll tests completed successfully!");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("-d") => {
            println!("Running demo test...");
            std::process::exit(run_demo_test());
        }
        Some("-f") => {
            println!("Running futex performance test...");
            std::process::exit(futex_perftest());
        }
        Some("-u") => {
            println!("Running UMCG performance test...");
            // Add UMCG performance test here if needed
            std::process::exit(0);
        }
        _ => {
            println!("Running full test suite...");
            run_full_tests();
        }
    }
}