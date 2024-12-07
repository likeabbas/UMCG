use std::sync::{Arc, Mutex, Condvar};
use std::time::{Duration, Instant};
use std::thread;

const UMCG_WORKER_ID_SHIFT: u64 = 5;
const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;

struct UmcgTestWorker {
    tid: u64,
}

struct UmcgTestServer {
    cur: u64,
    workers: u64,
}

struct CondvarTestSyncData {
    worker_task: bool,
    mutex: Mutex<bool>,
    cond: Condvar,
}

fn sys_umcg_ctl(
    flags: u64,
    cmd: u64,
    next_tid: u64,
    abs_timeout: u64,
    events: Option<&mut [u64]>,
    event_sz: usize,
) -> Result<(), std::io::Error> {
    // Placeholder implementation
    Ok(())
}

fn umcg_wait_retry(worker_id: u64, events: Option<&mut [u64]>, event_sz: usize) -> Result<(), std::io::Error> {
    let mut flags = 0;
    loop {
        match sys_umcg_ctl(flags, 4, worker_id >> UMCG_WORKER_ID_SHIFT, 0, events, event_sz) {
            Ok(()) => return Ok(()),
            Err(err) if err.raw_os_error() == Some(4) => {
                flags = UMCG_WAIT_FLAG_INTERRUPTED;
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

fn umcg_yield() {
    umcg_wait_retry(0, None, 0).expect("umcg_yield failed");
}

fn umcg_wait_any_worker(event: u64) -> u64 {
    const EVENT_SZ: usize = 2;
    let mut events = [0; EVENT_SZ];
    umcg_wait_retry(0, Some(&mut events), EVENT_SZ).expect("umcg_wait_any_worker failed");
    assert!((events[0] & UMCG_WORKER_EVENT_MASK) == event && events[1] == 0);
    events[0] & !UMCG_WORKER_EVENT_MASK
}

fn umcg_assert_worker_event(event: u64, worker_id: u64, event_type: u64) {
    assert!((event & UMCG_WORKER_EVENT_MASK) == event_type);
    assert!((event & !UMCG_WORKER_EVENT_MASK) == worker_id);
}

fn umcg_ctxsw_assert_worker_event(worker_id: u64, event: u64) {
    const EVENT_SZ: usize = 2;
    let mut events = [0; EVENT_SZ];
    sys_umcg_ctl(0, 5, worker_id >> UMCG_WORKER_ID_SHIFT, 0, Some(&mut events), EVENT_SZ).expect("umcg_ctxsw_assert_worker_event failed");
    umcg_assert_worker_event(events[0], worker_id, event);
    assert!(events[1] == 0);
}

static mut DONE: bool = false;

fn umcg_test_check_done(start: &Instant) {
    if start.elapsed().as_secs() >= 10 {
        unsafe {
            DONE = true;
        }
    }
}

fn umcg_demo_worker_a(server: Arc<Mutex<UmcgTestServer>>) {
    let tid = thread::current().id() as u64;
    println!("A == {}", tid);
    sys_umcg_ctl(0, 1, 0, tid << UMCG_WORKER_ID_SHIFT, None, 0).expect("umcg_ctl(A) failed");
    let mut i = 0;
    while !unsafe { DONE } {
        let x = i;
        i += 1;
        if x % 1000000 == 0 {
            print!(".");
            std::io::stdout().flush().unwrap();
        }
        if x % 10000000 == 0 {
            umcg_yield();
        }
    }
    println!("A == done");
    sys_umcg_ctl(0, 3, 0, 0, None, 0).expect("umcg_ctl(~A) failed");
}

fn umcg_demo_worker_b(server: Arc<Mutex<UmcgTestServer>>) {
    let tid = thread::current().id() as u64;
    println!("B == {}", tid);
    sys_umcg_ctl(0, 1, 0, tid << UMCG_WORKER_ID_SHIFT, None, 0).expect("umcg_ctl(B) failed");
    while !unsafe { DONE } {
        println!("B");
        thread::sleep(Duration::from_secs(1));
    }
    println!("B == done");
    sys_umcg_ctl(0, 3, 0, 0, None, 0).expect("umcg_ctl(~B) failed");
}

fn umcg_demo_worker_c(server: Arc<Mutex<UmcgTestServer>>) {
    let tid = thread::current().id() as u64;
    println!("C == {}", tid);
    sys_umcg_ctl(0, 1, 0, tid << UMCG_WORKER_ID_SHIFT, None, 0).expect("umcg_ctl(C) failed");
    while !unsafe { DONE } {
        println!("C");
        thread::sleep(Duration::from_secs(2));
    }
    println!("C == done");
    sys_umcg_ctl(0, 3, 0, 0, None, 0).expect("umcg_ctl(~C) failed");
}

pub fn umcg_demo() {
    let server = Arc::new(Mutex::new(UmcgTestServer { cur: 0, workers: 0 }));
    const WORKER_COUNT: usize = 3;
    let mut workers = Vec::with_capacity(WORKER_COUNT);

    sys_umcg_ctl(0, 2, 0, 0, None, 0).expect("umcg_ctl(server) failed");
    workers.push(thread::spawn(|| umcg_demo_worker_a(server.clone())));
    workers.push(thread::spawn(|| umcg_demo_worker_b(server.clone())));
    workers.push(thread::spawn(|| umcg_demo_worker_c(server.clone())));

    let start = Instant::now();
    while !unsafe { DONE } {
        let mut server = server.lock().unwrap();
        let cur = server.cur;
        if cur == 0 {
            print!("x");
            umcg_wait_retry(0, None, 0).expect("umcg_wait_retry failed");
        } else {
            println!("pick: {}", cur);
            umcg_ctxsw_assert_worker_event(cur, 1);
        }
        umcg_test_check_done(&start);
    }

    for worker in workers {
        worker.join().expect("worker join failed");
    }
}
