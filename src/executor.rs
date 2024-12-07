// use std::future::Future;
// use std::io;
// use std::sync::mpsc::{channel, Receiver, Sender};
// use std::sync::{Arc, Mutex};
// use std::collections::VecDeque;
// use crate::{umcg_syscall, Server, Thread, UmcgCmd};
// // Re-export the Thread type from our previous implementation
//
//
// pub struct UmcgExecutor {
//     tasks: Arc<Mutex<VecDeque<Task>>>,
//     workers: Vec<Thread>,
//     server: Arc<Server>,
//     task_sender: Sender<Task>,
//     task_receiver: Arc<Mutex<Receiver<Task>>>,
//     shutdown: Arc<Mutex<bool>>,
// }
//
// // Task representation
// type Task = Box<dyn FnOnce() + Send + 'static>;
//
// impl UmcgExecutor {
//     pub fn new(num_threads: usize) -> io::Result<Self> {
//         let (task_sender, task_receiver) = channel();
//         let task_receiver = Arc::new(Mutex::new(task_receiver));
//         let tasks = Arc::new(Mutex::new(VecDeque::new()));
//         let shutdown = Arc::new(Mutex::new(false));
//         let server = Arc::new(Server::new()?);
//
//         let mut workers = Vec::with_capacity(num_threads);
//
//         // Create worker threads
//         for _ in 0..num_threads {
//             let task_receiver = Arc::clone(&task_receiver);
//             let tasks = Arc::clone(&tasks);
//             let shutdown = Arc::clone(&shutdown);
//             let server = Arc::clone(&server);
//
//             let worker = Thread::spawn(move || {
//                 Worker::new(task_receiver, tasks, shutdown, server).run();
//             })?;
//
//             workers.push(worker);
//         }
//
//         Ok(UmcgExecutor {
//             tasks,
//             workers,
//             server,
//             task_sender,
//             task_receiver,
//             shutdown,
//         })
//     }
//
//     pub fn spawn<F>(&self, f: F)
//     where
//         F: FnOnce() + Send + 'static,
//     {
//         let task = Box::new(f);
//         self.task_sender.send(task).expect("Failed to send task");
//     }
//
//     pub fn spawn_with_handle<F, T>(&self, f: F) -> JoinHandle<T>
//     where
//         F: FnOnce() -> T + Send + 'static,
//         T: Send + 'static,
//     {
//         let (result_sender, result_receiver) = channel();
//
//         let task = Box::new(move || {
//             let result = f();
//             let _ = result_sender.send(result);
//         });
//
//         self.task_sender.send(task).expect("Failed to send task");
//
//         JoinHandle {
//             receiver: result_receiver,
//         }
//     }
// }
//
// impl Drop for UmcgExecutor {
//     fn drop(&mut self) {
//         // Signal shutdown
//         *self.shutdown.lock().unwrap() = true;
//
//         // Wake up all workers
//         for _ in &self.workers {
//             self.spawn(|| {});
//         }
//
//         // Wait for all workers to finish
//         for worker in self.workers.drain(..) {
//             let _ = worker.join();
//         }
//     }
// }
//
// struct Worker {
//     task_receiver: Arc<Mutex<Receiver<Task>>>,
//     tasks: Arc<Mutex<VecDeque<Task>>>,
//     shutdown: Arc<Mutex<bool>>,
//     server: Arc<Server>,
// }
//
// impl Worker {
//     fn new(
//         task_receiver: Arc<Mutex<Receiver<Task>>>,
//         tasks: Arc<Mutex<VecDeque<Task>>>,
//         shutdown: Arc<Mutex<bool>>,
//         server: Arc<Server>,
//     ) -> Self {
//         Worker {
//             task_receiver,
//             tasks,
//             shutdown,
//             server,
//         }
//     }
//
//     fn run(&self) {
//         while !*self.shutdown.lock().unwrap() {
//             // Try to get a task
//             let task = {
//                 let mut tasks = self.tasks.lock().unwrap();
//                 if let Some(task) = tasks.pop_front() {
//                     Some(task)
//                 } else {
//                     // If no tasks in the queue, try to receive new ones
//                     match self.task_receiver.lock().unwrap().try_recv() {
//                         Ok(task) => Some(task),
//                         Err(_) => None,
//                     }
//                 }
//             };
//
//             match task {
//                 Some(task) => {
//                     // Execute the task
//                     task();
//                 }
//                 None => {
//                     // No tasks available, yield to other workers
//                     // This is where UMCG's cooperative scheduling comes in
//                     unsafe {
//                         umcg_syscall(
//                             0,
//                             UmcgCmd::Wait as u64,
//                             0,
//                             0,
//                             std::ptr::null_mut(),
//                             0,
//                         ).expect("Failed to yield");
//                     }
//                 }
//             }
//         }
//     }
// }
//
// pub struct JoinHandle<T> {
//     receiver: Receiver<T>,
// }
//
// impl<T> JoinHandle<T> {
//     pub fn join(self) -> io::Result<T> {
//         self.receiver.recv().map_err(|e| {
//             io::Error::new(io::ErrorKind::Other, e.to_string())
//         })
//     }
// }
//
// // Example usage:
// #[cfg(test)]
// mod tests {
//     use super::*;
//     use std::sync::atomic::{AtomicUsize, Ordering};
//     use std::time::Duration;
//
//     #[test]
//     fn test_executor() {
//         let executor = UmcgExecutor::new(4).unwrap();
//         let counter = Arc::new(AtomicUsize::new(0));
//
//         // Spawn 100 tasks
//         for _ in 0..100 {
//             let counter = counter.clone();
//             executor.spawn(move || {
//                 counter.fetch_add(1, Ordering::SeqCst);
//             });
//         }
//
//         // Give some time for tasks to complete
//         std::thread::sleep(Duration::from_millis(100));
//
//         assert_eq!(counter.load(Ordering::SeqCst), 100);
//     }
//
//     #[test]
//     fn test_spawn_with_handle() {
//         let executor = UmcgExecutor::new(4).unwrap();
//
//         let handle = executor.spawn_with_handle(|| 42);
//         assert_eq!(handle.join().unwrap(), 42);
//     }
// }