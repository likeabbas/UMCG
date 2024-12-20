// use std::sync::atomic::{AtomicBool, Ordering};
// use std::sync::Arc;
// use std::thread;
// use std::time::{Duration, SystemTime, UNIX_EPOCH};
// use libc::{self, pid_t, syscall, SYS_gettid};
// use chrono::Local;
// use std::collections::VecDeque;
//
// const SYS_UMCG_CTL: i64 = 450;
// const UMCG_WORKER_ID_SHIFT: u64 = 5;
// const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
// const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;
//
// #[derive(Debug, Copy, Clone)]
// #[repr(u64)]
// enum UmcgCmd {
//     RegisterWorker = 1,
//     RegisterServer = 2,
//     Unregister = 3,
//     Wake = 4,
//     Wait = 5,
//     CtxSwitch = 6,
// }
//
// #[derive(Debug, PartialEq, Copy, Clone)]
// #[repr(u64)]
// enum UmcgEventType {
//     Block = 1,
//     Wake = 2,
//     Wait = 3,
//     Exit = 4,
//     Timeout = 5,
//     Preempt = 6,
// }
//
// impl UmcgEventType {
//     fn from_u64(value: u64) -> Option<UmcgEventType> {
//         match value {
//             1 => Some(UmcgEventType::Block),
//             2 => Some(UmcgEventType::Wake),
//             3 => Some(UmcgEventType::Wait),
//             4 => Some(UmcgEventType::Exit),
//             5 => Some(UmcgEventType::Timeout),
//             6 => Some(UmcgEventType::Preempt),
//             _ => None,
//         }
//     }
// }
//
// fn log(msg: &str) {
//     let timestamp = Local::now().format("%H:%M:%S.%3f");
//     println!("[{}] {}", timestamp, msg);
// }
//
// type Task = Box<dyn FnOnce() + Send>;
//
// #[derive(Debug, PartialEq)]
// enum WorkerState {
//     New,
//     Running,
//     Blocked,
//     Done,
// }
//
// struct Worker {
//     tid: pid_t,
//     worker_id: u64,
//     state: WorkerState,
// }
//
// pub struct UmcgScheduler {
//     tasks: VecDeque<Task>,
//     workers: Vec<Worker>,
//     shutdown: Arc<AtomicBool>,
//     completed_tasks: usize,
// }
//
// impl UmcgScheduler {
//     fn new() -> Self {
//         let mut tasks = VecDeque::new();
//
//         // Create tasks with decreasing sleep times from 10s to 1s
//         for i in 0..10 {
//             let duration = Duration::from_secs(10 - i as u64);
//             let task: Task = Box::new(move || {
//                 let tid = unsafe { syscall(SYS_gettid) } as pid_t;
//                 log(&format!("Worker[{}] starting {} second sleep", tid, duration.as_secs()));
//                 thread::sleep(duration);
//                 log(&format!("Worker[{}] completed {} second sleep", tid, duration.as_secs()));
//             });
//             tasks.push_back(task);
//         }
//
//         UmcgScheduler {
//             tasks,
//             workers: Vec::new(),
//             shutdown: Arc::new(AtomicBool::new(false)),
//             completed_tasks: 0,
//         }
//     }
//
//     fn sys_umcg_ctl(
//         flags: u64,
//         cmd: UmcgCmd,
//         next_tid: pid_t,
//         abs_timeout: u64,
//         events: Option<&mut [u64]>,
//         event_sz: i32,
//     ) -> i32 {
//         unsafe {
//             syscall(
//                 SYS_UMCG_CTL,
//                 flags as i64,
//                 cmd as i64,
//                 next_tid as i64,
//                 abs_timeout as i64,
//                 events.map_or(std::ptr::null_mut(), |e| e.as_mut_ptr()) as i64,
//                 event_sz as i64,
//             ) as i32
//         }
//     }
//
//     fn wait_for_worker_event(&self) -> Option<(u64, UmcgEventType)> {
//         let mut events = [0u64; 2];
//         match Self::sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, Some(&mut events), 2) {
//             0 => {
//                 let event = events[0];
//                 if event == 0 {
//                     return None;
//                 }
//                 let event_type = event & UMCG_WORKER_EVENT_MASK;
//                 let worker_id = event >> UMCG_WORKER_ID_SHIFT;
//                 UmcgEventType::from_u64(event_type).map(|evt| (worker_id, evt))
//             },
//             _ => None
//         }
//     }
//
//     fn switch_to_worker(&self, worker_tid: pid_t, events: &mut [u64; 2]) -> bool {
//         match Self::sys_umcg_ctl(0, UmcgCmd::CtxSwitch, worker_tid, 0, Some(events), 2) {
//             0 => true,
//             _ => false
//         }
//     }
//
//     fn worker_thread() {
//         let tid = unsafe { syscall(SYS_gettid) } as pid_t;
//         let worker_id = (tid as u64) << UMCG_WORKER_ID_SHIFT;
//
//         log(&format!("Worker[{}] registering", tid));
//
//         // Register with UMCG
//         Self::sys_umcg_ctl(0, UmcgCmd::RegisterWorker, 0, worker_id, None, 0);
//
//         // Initial wait - this generates Wake event to server
//         Self::sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 0);
//
//         // Task execution loop
//         while !SHOULD_EXIT.with(|e| *e.borrow()) {
//             if let Some(task) = CURRENT_TASK.with(|t| t.borrow_mut().take()) {
//                 task();
//                 // Signal completion
//                 Self::sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 0);
//             }
//
//             // Wait for next task
//             if Self::sys_umcg_ctl(0, UmcgCmd::Wait, 0, 0, None, 0) < 0 {
//                 break;
//             }
//         }
//
//         log(&format!("Worker[{}] unregistering", tid));
//         Self::sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
//     }
//
//     pub fn run(&mut self) {
//         let server_tid = unsafe { syscall(SYS_gettid) } as pid_t;
//         log(&format!("Server[{}] starting", server_tid));
//
//         // Register as UMCG server
//         Self::sys_umcg_ctl(0, UmcgCmd::RegisterServer, 0, 0, None, 0);
//         log(&format!("Server[{}] registered", server_tid));
//
//         // Create worker threads
//         let mut worker_threads = Vec::new();
//         for _ in 0..10 {
//             worker_threads.push(thread::spawn(|| Self::worker_thread()));
//         }
//
//         // Main event loop
//         let mut events = [0u64; 2];
//
//         while self.completed_tasks < 10 {
//             match self.wait_for_worker_event() {
//                 Some((worker_id, UmcgEventType::Wake)) => {
//                     let worker_tid = (worker_id as pid_t);
//                     log(&format!("Server[{}] received Wake from Worker[{}]", server_tid, worker_tid));
//
//                     // Add new worker or find existing one
//                     if !self.workers.iter().any(|w| w.worker_id == worker_id) {
//                         self.workers.push(Worker {
//                             tid: worker_tid,
//                             worker_id,
//                             state: WorkerState::New,
//                         });
//
//                         log(&format!("Server[{}] switching to Worker[{}]", server_tid, worker_tid));
//                         if self.switch_to_worker(worker_tid, &mut events) {
//                             if let Some(w) = self.workers.iter_mut().find(|w| w.worker_id == worker_id) {
//                                 w.state = WorkerState::Blocked;
//                                 log(&format!("Server[{}] Worker[{}] entered Block state", server_tid, worker_tid));
//                             }
//                         }
//                     } else if let Some(task) = self.tasks.pop_front() {
//                         // Existing worker woke up and ready for task
//                         CURRENT_TASK.with(|t| *t.borrow_mut() = Some(task));
//                         if self.switch_to_worker(worker_tid, &mut events) {
//                             if let Some(w) = self.workers.iter_mut().find(|w| w.worker_id == worker_id) {
//                                 w.state = WorkerState::Running;
//                                 log(&format!("Server[{}] assigned task to Worker[{}]", server_tid, worker_tid));
//                             }
//                         } else {
//                             log(&format!("Server[{}] failed to assign task to Worker[{}]", server_tid, worker_tid));
//                             self.tasks.push_front(CURRENT_TASK.with(|t| t.borrow_mut().take().unwrap()));
//                         }
//                     }
//                 },
//                 Some((worker_id, UmcgEventType::Block)) => {
//                     let worker_tid = (worker_id as pid_t);
//                     if let Some(w) = self.workers.iter_mut().find(|w| w.worker_id == worker_id) {
//                         w.state = WorkerState::Blocked;
//                         log(&format!("Server[{}] Worker[{}] blocked", server_tid, worker_tid));
//                     }
//                 },
//                 Some((worker_id, UmcgEventType::Wait)) => {
//                     let worker_tid = (worker_id as pid_t);
//                     if let Some(w) = self.workers.iter_mut().find(|w| w.worker_id == worker_id) {
//                         if w.state == WorkerState::Running {
//                             self.completed_tasks += 1;
//                             w.state = WorkerState::Done;
//                             log(&format!("Server[{}] Worker[{}] completed task ({}/10)",
//                                          server_tid, worker_tid, self.completed_tasks));
//                         }
//                     }
//                 },
//                 _ => {}
//             }
//         }
//
//         // Shutdown
//         log(&format!("Server[{}] completed all tasks, shutting down", server_tid));
//         self.shutdown.store(true, Ordering::Relaxed);
//         SHOULD_EXIT.with(|e| *e.borrow_mut() = true);
//
//         // Wait for all workers to complete
//         for thread in worker_threads {
//             thread.join().unwrap();
//         }
//
//         // Unregister server
//         Self::sys_umcg_ctl(0, UmcgCmd::Unregister, 0, 0, None, 0);
//         log(&format!("Server[{}] unregistered", server_tid));
//     }
// }
//
// thread_local! {
//     static CURRENT_TASK: std::cell::RefCell<Option<Task>> = std::cell::RefCell::new(None);
//     static SHOULD_EXIT: std::cell::RefCell<bool> = std::cell::RefCell::new(false);
// }
//
// pub fn run_umcg_scheduler() {
//     let mut scheduler = UmcgScheduler::new();
//     scheduler.run();
// }