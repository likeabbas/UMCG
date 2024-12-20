// // use std::net::{TcpStream, TcpListener};
// // use std::io::{self, Read, Write, BufRead, BufReader};
// // use std::thread;
// // use std::fs::File;
// // use UMCG::ShardedExecutor;
// //
// // fn handle_read(mut stream: &TcpStream) {
// //     let mut buf = [0u8; 4096];
// //     match stream.read(&mut buf) {
// //         Ok(n) => {
// //             let req_str = String::from_utf8_lossy(&buf[..n]);
// //             println!("Received request ({} bytes): {}", n, req_str);
// //         },
// //         Err(e) => println!("Unable to read stream: {}", e),
// //     }
// // }
// //
// // fn handle_write(mut stream: TcpStream, message: String) {
// //     let response = format!(
// //         "HTTP/1.1 200 OK\r\n\
// //          Content-Type: text/html; charset=UTF-8\r\n\
// //          Content-Length: {}\r\n\
// //          \r\n\
// //          {}",
// //         message.len(),
// //         message
// //     );
// //
// //     match stream.write(response.as_bytes()) {
// //         Ok(n) => println!("Response sent: {} bytes", n),
// //         Err(e) => println!("Failed sending response: {}", e),
// //     }
// //     if let Err(e) = stream.flush() {
// //         println!("Failed to flush stream: {}", e);
// //     }
// // }
// //
// // fn handle_client(stream: TcpStream, executor: &ShardedExecutor) {
// //     if let Ok(addr) = stream.peer_addr() {
// //         println!("New connection from: {}", addr);
// //     }
// //
// //     handle_read(&stream);
// //
// //     let (tx, rx) = std::sync::mpsc::channel();
// //
// //     println!("About to spawn UMCG worker");
// //     executor.spawn(0, move || {
// //         println!("UMCG worker running");
// //         if let Err(e) = tx.send("UMCG worker completed task".to_string()) {
// //             println!("Failed to send completion message: {:?}", e);
// //         }
// //     });
// //     println!("Spawned worker");
// //
// //     println!("Waiting for worker completion");
// //     let worker_message = rx.recv().unwrap_or_else(|e| {
// //         println!("Worker communication failed: {:?}", e);
// //         "Worker failed to complete".to_string()
// //     });
// //
// //     println!("Preparing response");
// //     let response = format!("<html><body>\
// //         <p>Hello world</p>\
// //         <p>{}</p>\
// //         </body></html>", worker_message);
// //
// //     handle_write(stream, response);
// // }
// //
// // fn main() {
// //     find_umcg_syscalls();
// //     check_syscalls();
// //     println!("Starting server...");
// //
// //     // Create a UMCG executor with one core
// //     println!("Creating UMCG executor");
// //     let executor = match ShardedExecutor::new(1) {
// //         Ok(e) => {
// //             println!("Successfully created UMCG executor");
// //             e
// //         },
// //         Err(e) => {
// //             println!("Failed to create UMCG executor: {:?}", e);
// //             panic!("Failed to create UMCG executor")
// //         }
// //     };
// //     let executor = std::sync::Arc::new(executor);
// //     println!("UMCG executor ready");
// //
// //     println!("Attempting to bind to 0.0.0.0:8080...");
// //     match TcpListener::bind("0.0.0.0:8080") {
// //         Ok(listener) => {
// //             if let Ok(addr) = listener.local_addr() {
// //                 println!("Server successfully bound and listening on: {}", addr);
// //             }
// //
// //             println!("Waiting for connections...");
// //             for stream in listener.incoming() {
// //                 match stream {
// //                     Ok(stream) => {
// //                         let executor = executor.clone();
// //                         thread::spawn(move || {
// //                             handle_client(stream, &executor)
// //                         });
// //                     }
// //                     Err(e) => {
// //                         println!("Error accepting connection: {}", e);
// //                     }
// //                 }
// //             }
// //         }
// //         Err(e) => {
// //             println!("Failed to bind to 0.0.0.0:8080: {}", e);
// //         }
// //     }
// // }
// //
// // unsafe fn test_syscall(num: i32, name: &str) {
// //     let ret: i64;
// //     std::arch::asm!(
// //     "syscall",
// //     in("rax") num,    // syscall number
// //     in("rdi") 0,      // dummy args
// //     in("rsi") 0,
// //     in("rdx") 0,
// //     in("r10") 0,
// //     in("r8") 0,
// //     in("r9") 0,
// //     out("rcx") _,
// //     out("r11") _,
// //     lateout("rax") ret,
// //     );
// //     println!("Syscall {} ({}) returned: {} ({})",
// //              num,
// //              name,
// //              ret,
// //              if ret < 0 {
// //                  io::Error::from_raw_os_error(-ret as i32).to_string()
// //              } else {
// //                  "available".to_string()
// //              }
// //     );
// // }
// //
// // fn check_syscalls() {
// //     println!("Checking available syscalls:");
// //     unsafe {
// //         test_syscall(450, "UMCG"); // Our UMCG syscall
// //         test_syscall(449, "UMCG-1"); // Try the one below
// //         test_syscall(451, "UMCG+1"); // Try the one above
// //         test_syscall(1, "write"); // Known good syscall for comparison
// //     }
// // }
// //
// // fn find_umcg_syscalls() {
// //     println!("Searching for UMCG syscalls...");
// //
// //     // Try reading kernel symbols
// //     if let Ok(file) = File::open("/proc/kallsyms") {
// //         let reader = BufReader::new(file);
// //         println!("Found kernel symbols:");
// //
// //         for line in reader.lines() {
// //             if let Ok(line) = line {
// //                 if line.contains("umcg") {
// //                     println!("{}", line);
// //                 }
// //             }
// //         }
// //     }
// //
// //     // Could also check syscall table if available
// //     if let Ok(file) = File::open("/usr/include/asm/unistd_64.h") {
// //         let reader = BufReader::new(file);
// //         println!("\nFound syscall definitions:");
// //
// //         for line in reader.lines() {
// //             if let Ok(line) = line {
// //                 if line.contains("umcg") {
// //                     println!("{}", line);
// //                 }
// //             }
// //         }
// //     }
// // }
//
use libc::{self, c_int, pid_t, syscall, SYS_gettid, CLOCK_REALTIME, timespec, EINTR, EINVAL, ETIMEDOUT, ESRCH, EFAULT, EAGAIN};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::ptr;
use std::io::Write;
use UMCG::{run_original_demo};
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

fn umcg_perftest() -> i32 {
    let mut cycles = 0i64;
    let worker = thread::spawn(|| {
        let tid = get_thread_id();
        assert_eq!(
            sys_umcg_ctl(
                0,
                UmcgCmd::RegisterWorker,
                0,
                ((tid as u64) << UMCG_WORKER_ID_SHIFT),
                None,
                0
            ),
            0
        );
        while !DONE.load(Ordering::Relaxed) {
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 0), 0);
        }
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
    });

    println!("UMCG performance test duration: 10 seconds...");
    assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

    // Wait for worker to register
    let worker_id = umcg_wait_any_worker(UmcgEventType::Wake);
    let start = SystemTime::now();

    while !DONE.load(Ordering::Relaxed) {
        umcg_ctxsw_assert_worker_event(worker_id, UmcgEventType::Wait);
        cycles += 1;

        if start.elapsed().unwrap() >= Duration::from_secs(10) {
            DONE.store(true, Ordering::Relaxed);
        }
    }

    let duration = start.elapsed().unwrap().as_secs_f64();
    println!(
        "Results: {} cycles ({} cycles/sec)",
        cycles,
        cycles as f64 / duration
    );

    umcg_ctxsw_assert_worker_event(worker_id, UmcgEventType::Exit);
    worker.join().unwrap();
    0
}

fn run_full_tests() {

    println!("Running scheduler");
    run_original_demo();

    println!("Running UMCG test suite...");

    println!("Running umcg perf test");
    umcg_perftest();

    println!("\nRunning basic tests...");
    umcg_test_basic();
    run_basic_tests();
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
//
// // fn run_io_test() {
// //     println!("\nRunning IO blocking test...");
// //     assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
// //     println!("Server registered");
// //
// //     let progress_counter = Arc::new(AtomicI32::new(0));
// //     let done = Arc::new(AtomicBool::new(false));
// //
// //     // Create "busy" worker that just counts
// //     let progress = progress_counter.clone();
// //     let done_flag = done.clone();
// //     let counter_handle = thread::spawn(move || {
// //         let tid = get_thread_id();
// //         println!("Counter worker starting on tid {}", tid);
// //         assert_eq!(
// //             sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
// //             0
// //         );
// //
// //         while !done_flag.load(Ordering::Relaxed) {
// //             progress.fetch_add(1, Ordering::Relaxed);
// //             if progress.load(Ordering::Relaxed) % 1000000 == 0 {
// //                 print!(".");
// //                 std::io::stdout().flush().unwrap();
// //             }
// //             if progress.load(Ordering::Relaxed) % 10000000 == 0 {
// //                 umcg_yield();
// //             }
// //         }
// //
// //         assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //         println!("\nCounter worker finished");
// //     });
// //
// //     // Create IO worker that tries to connect to non-existent servers
// //     let done_flag = done.clone();
// //     let io_handle = thread::spawn(move || {
// //         let tid = get_thread_id();
// //         println!("IO worker starting on tid {}", tid);
// //         assert_eq!(
// //             sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
// //             0
// //         );
// //
// //         let mut attempt = 0;
// //         while !done_flag.load(Ordering::Relaxed) {
// //             attempt += 1;
// //             println!("\nIO worker attempting connection #{}", attempt);
// //
// //             // Try to connect to a non-existent server - this will block
// //             match std::net::TcpStream::connect("10.0.0.1:12345") {
// //                 Ok(_) => println!("Unexpected connection success"),
// //                 Err(e) => println!("Expected connection error: {}", e),
// //             }
// //
// //             // Sleep a bit before next attempt
// //             thread::sleep(Duration::from_millis(500));
// //         }
// //
// //         assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //         println!("IO worker finished");
// //     });
// //
// //     // Let the test run for a few seconds
// //     println!("Test running - watching for progress while IO blocks...");
// //     thread::sleep(Duration::from_secs(5));
// //
// //     // Check progress
// //     let final_count = progress_counter.load(Ordering::Relaxed);
// //     println!("\nProgress counter reached: {}", final_count);
// //     assert!(final_count > 0, "Counter worker made no progress!");
// //
// //     // Clean up
// //     done.store(true, Ordering::Relaxed);
// //     counter_handle.join().unwrap();
// //     io_handle.join().unwrap();
// //
// //     assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //     println!("IO blocking test completed successfully!");
// // }
//
// // fn run_io_test() {
// //     println!("\nRunning IO blocking test...");
// //
// //     // Create a structure similar to demo test server
// //     struct TestServer {
// //         node: std::collections::VecDeque<pid_t>,
// //         cur: pid_t,
// //     }
// //
// //     let mut server = TestServer {
// //         node: std::collections::VecDeque::new(),
// //         cur: 0,
// //     };
// //
// //     assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);
// //     println!("Server registered");
// //
// //     let progress_counter = Arc::new(AtomicI32::new(0));
// //     let done = Arc::new(AtomicBool::new(false));
// //
// //     // Create "busy" worker that just counts
// //     let progress = progress_counter.clone();
// //     let done_flag = done.clone();
// //     let counter_handle = thread::spawn(move || {
// //         let tid = get_thread_id();
// //         println!("Counter worker starting on tid {}", tid);
// //         assert_eq!(
// //             sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
// //             0
// //         );
// //
// //         while !done_flag.load(Ordering::Relaxed) {
// //             progress.fetch_add(1, Ordering::Relaxed);
// //             if progress.load(Ordering::Relaxed) % 1000000 == 0 {
// //                 print!(".");
// //                 std::io::stdout().flush().unwrap();
// //             }
// //             if progress.load(Ordering::Relaxed) % 10000000 == 0 {
// //                 umcg_yield();
// //             }
// //         }
// //
// //         assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //         println!("\nCounter worker finished");
// //     });
// //
// //     // Create IO worker that tries to connect to non-existent servers
// //     let done_flag = done.clone();
// //     let io_handle = thread::spawn(move || {
// //         let tid = get_thread_id();
// //         println!("IO worker starting on tid {}", tid);
// //         assert_eq!(
// //             sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, ((tid as u64) << UMCG_WORKER_ID_SHIFT), None, 0),
// //             0
// //         );
// //
// //         let mut attempt = 0;
// //         while !done_flag.load(Ordering::Relaxed) {
// //             attempt += 1;
// //             println!("\nIO worker attempting connection #{}", attempt);
// //
// //             // Try to connect to a non-existent server - this will block
// //             match std::net::TcpStream::connect("10.0.0.1:12345") {
// //                 Ok(_) => println!("Unexpected connection success"),
// //                 Err(e) => println!("Expected connection error: {}", e),
// //             }
// //
// //             // Sleep a bit before next attempt
// //             thread::sleep(Duration::from_millis(500));
// //         }
// //
// //         assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //         println!("IO worker finished");
// //     });
// //
// //     let start = SystemTime::now();
// //     let test_duration = Duration::from_secs(5);
// //     let mut events = [0u64; 4];
// //
// //     println!("Server loop starting...");
// //     while !done.load(Ordering::Relaxed) {
// //         // Pick next worker from queue
// //         server.cur = server.node.pop_front().unwrap_or(0);
// //
// //         let ret = if server.cur == 0 {
// //             // No worker running, wait for events
// //             println!("Server waiting for events...");
// //             umcg_wait_retry(0, Some(&mut events), 4)
// //         } else {
// //             // Context switch to worker
// //             println!("Server switching to worker {}", server.cur);
// //             sys_umcg_ctl(0, UmcgCmd::CtxSwitch, server.cur, 0, Some(&mut events), 4)
// //         };
// //
// //         if ret != 0 {
// //             println!("Server loop error");
// //             break;
// //         }
// //
// //         for i in 0..4 {
// //             let event = events[i];
// //             if event == 0 {
// //                 break;
// //             }
// //
// //             let event_type = event & UMCG_WORKER_EVENT_MASK;
// //             let tid = event >> UMCG_WORKER_ID_SHIFT;
// //             println!("Worker {} event {}", tid, event_type);
// //
// //             // Handle events similar to demo test
// //             match event_type {
// //                 2 => { // WAKE event
// //                     println!("Worker {} woke up", tid);
// //                     server.node.push_back(tid as pid_t);
// //                 }
// //                 3 => { // WAIT event
// //                     println!("Worker {} yielded", tid);
// //                     // Only add back to queue if we're not trying to shut down
// //                     if !done.load(Ordering::Relaxed) {
// //                         server.node.push_back(tid as pid_t);
// //                     }
// //                 }
// //                 1 => { // BLOCK event
// //                     println!("Worker {} blocked", tid);
// //                     // Don't add blocked workers back to queue
// //                 }
// //                 4 => { // EXIT event
// //                     println!("Worker {} exited", tid);
// //                 }
// //                 _ => {
// //                     println!("Other event type: {} from worker {}", event_type, tid);
// //                 }
// //             }
// //
// //             // Also add check for empty queue and no events
// //             if done.load(Ordering::Relaxed) && server.node.is_empty() {
// //                 println!("No more workers and shutdown requested, exiting server loop");
// //                 break;
// //             }
// //         }
// //
// //         if start.elapsed().unwrap() >= test_duration {
// //             done.store(true, Ordering::Relaxed);
// //         }
// //     }
// //
// //     // Check progress
// //     let final_count = progress_counter.load(Ordering::Relaxed);
// //     println!("\nProgress counter reached: {}", final_count);
// //     assert!(final_count > 0, "Counter worker made no progress!");
// //
// //     // Clean up
// //     counter_handle.join().unwrap();
// //     io_handle.join().unwrap();
// //
// //     assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
// //     println!("IO blocking test completed successfully!");
// // }
