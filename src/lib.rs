use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread::{self, JoinHandle};
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

enum WorkerTask {
    Function(Box<dyn FnOnce() + Send>),
    Shutdown,
}

struct Worker {
    id: usize,
    tid: pid_t,
    task_rx: Receiver<WorkerTask>,
    task_tx: Sender<WorkerTask>,
}

impl Worker {
    fn new(id: usize) -> Self {
        let (task_tx, task_rx) = channel();
        Self {
            id,
            tid: 0,
            task_rx,
            task_tx,
        }
    }

    fn get_task_sender(&self) -> Sender<WorkerTask> {
        self.task_tx.clone()
    }

    fn start(mut self, workers_count: Arc<AtomicI32>, done: Arc<AtomicBool>) -> JoinHandle<()> {
        thread::spawn(move || {
            self.tid = get_thread_id();
            log_with_timestamp(&format!("Worker {} (tid: {}) starting", self.id, self.tid));

            // Register worker
            assert_eq!(
                sys_umcg_ctl(
                    0,
                    UmcgCmd::RegisterWorker,
                    0,
                    (self.tid as u64) << UMCG_WORKER_ID_SHIFT,
                    None,
                    0
                ),
                0
            );

            workers_count.fetch_add(1, Ordering::Relaxed);

            // Worker event loop
            while !done.load(Ordering::Relaxed) {
                match self.task_rx.try_recv() {
                    Ok(task) => {
                        match task {
                            WorkerTask::Function(task) => {
                                task();
                            }
                            WorkerTask::Shutdown => {
                                log_with_timestamp(&format!("Worker {}: Received shutdown signal", self.id));
                                break;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // No more tasks available right now
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Channel closed
                        break;
                    }
                }
            }

            log_with_timestamp(&format!("Worker {}: Shutting down", self.id));
            workers_count.fetch_sub(1, Ordering::Relaxed);
            assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
        })
    }
}

struct Server {
    runnable_workers: VecDeque<i32>,
    completed_cycles: HashMap<u64, bool>,
    worker_count: usize,
    workers_count: Arc<AtomicI32>,
    done: Arc<AtomicBool>,
    task_senders: HashMap<i32, Sender<WorkerTask>>,
}

impl Server {
    fn new(worker_count: usize) -> Self {
        Self {
            runnable_workers: VecDeque::new(),
            completed_cycles: HashMap::new(),
            worker_count,
            workers_count: Arc::new(AtomicI32::new(0)),
            done: Arc::new(AtomicBool::new(false)),
            task_senders: HashMap::new(),
        }
    }

    fn get_workers_count(&self) -> Arc<AtomicI32> {
        self.workers_count.clone()
    }

    fn get_done_flag(&self) -> Arc<AtomicBool> {
        self.done.clone()
    }

    fn register_worker_sender(&mut self, worker_tid: i32, sender: Sender<WorkerTask>) {
        self.task_senders.insert(worker_tid, sender);
    }

    fn send_task_to_worker(&self, worker_tid: i32, task: WorkerTask) -> bool {
        if let Some(sender) = self.task_senders.get(&worker_tid) {
            sender.send(task).is_ok()
        } else {
            false
        }
    }

    fn process_event(&mut self, event: u64) {
        let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
        let worker_tid = event >> UMCG_WORKER_ID_SHIFT;

        match event_type {
            1 => { // BLOCK
                log_with_timestamp(&format!("Server: Worker {} blocked", worker_tid));
                if let Some(pos) = self.runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                    self.runnable_workers.remove(pos);
                }
            },
            2 => { // WAKE
                log_with_timestamp(&format!("Server: Worker {} woke up", worker_tid));
                if !self.runnable_workers.contains(&(worker_tid as i32)) {
                    self.runnable_workers.push_back(worker_tid as i32);
                }
            },
            3 => { // WAIT
                log_with_timestamp(&format!("Server: Worker {} yielded", worker_tid));
                if let Some(pos) = self.runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                    self.runnable_workers.remove(pos);
                    self.runnable_workers.push_back(worker_tid as i32);
                }
            },
            4 => { // EXIT
                log_with_timestamp(&format!("Server: Worker {} exited", worker_tid));
                if let Some(pos) = self.runnable_workers.iter().position(|&x| x == worker_tid as i32) {
                    self.runnable_workers.remove(pos);
                }
                self.completed_cycles.insert(worker_tid, true);

                if self.completed_cycles.len() == self.worker_count {
                    self.done.store(true, Ordering::Relaxed);
                }
            },
            _ => log_with_timestamp(&format!("Server: Unknown event {} from worker {}", event_type, worker_tid)),
        }

        // Implement round-robin scheduling
        if self.runnable_workers.front() == Some(&(worker_tid as i32)) {
            if let Some(worker) = self.runnable_workers.pop_front() {
                self.runnable_workers.push_back(worker);
            }
        }
    }

    fn run(&mut self) -> i32 {
        assert_eq!(sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0), 0);

        let test_start = SystemTime::now();

        while !self.done.load(Ordering::Relaxed) || self.workers_count.load(Ordering::Relaxed) != 0 {
            let mut events = [0u64; 6];
            let ret = if let Some(&next_worker) = self.runnable_workers.front() {
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

            for &event in events.iter().take_while(|&&e| e != 0) {
                self.process_event(event);

                let event_type = event & ((1 << UMCG_WORKER_ID_SHIFT) - 1);
                if event_type == 4 && self.completed_cycles.len() == self.worker_count {
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

        assert_eq!(sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0), 0);
        log_with_timestamp("UMCG workers demo completed");
        0
    }
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

pub fn run_umcg_workers<F>(worker_count: usize, worker_fn: F) -> i32
where
    F: Fn(usize) + Send + Clone + 'static
{
    log_with_timestamp(&format!("Starting UMCG demo with {} workers...", worker_count));

    let mut server = Server::new(worker_count);
    let mut handles = Vec::new();

    // Create and start workers
    for i in 0..worker_count {
        let worker = Worker::new(i);
        let task_sender = worker.get_task_sender();

        // Send initial task
        let worker_fn = worker_fn.clone();
        task_sender.send(WorkerTask::Function(Box::new(move || worker_fn(i)))).unwrap();

        server.register_worker_sender(i as i32, task_sender);

        handles.push(worker.start(server.get_workers_count(), server.get_done_flag()));
    }

    // Run server
    let result = server.run();

    // No need to send shutdown signals anymore as workers exit after their task

    // Clean up
    for handle in handles {
        handle.join().unwrap();
    }

    result
}

pub fn run_original_demo() -> i32 {
    run_umcg_workers(3, |id| {
        // Each worker will get two single-sleep tasks
        for task_num in 0..2 {
            let sleep_duration = match id {
                0 => 2, // Worker A tasks: 2s each
                1 => 3, // Worker B tasks: 3s each
                _ => 4, // Worker C tasks: 4s each
            };

            log_with_timestamp(&format!("Worker {}: Starting sleep for task {}", id, task_num));
            thread::sleep(Duration::from_secs(sleep_duration));
            log_with_timestamp(&format!("Worker {}: Finished sleep for task {}", id, task_num));
        }
    })
}