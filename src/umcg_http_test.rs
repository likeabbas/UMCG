// use std::sync::Arc;
// use tiny_http::{Server, Response};
// use crate::{UMCGExecutor, TaskHandle, log_with_timestamp};
//
// pub struct UMCGHttpServer {
//     server: Server,
//     executor: Arc<UMCGExecutor>,
// }
//
// impl UMCGHttpServer {
//     pub fn new<A>(addr: A) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
//     where
//         A: std::net::ToSocketAddrs,
//     {
//         // Create executor with more workers
//         let executor = UMCGExecutor::new(1, 4); // More servers to handle multiple workers
//         Ok(UMCGHttpServer {
//             server: Server::http(addr)?,
//             executor,
//         })
//     }
//
//     pub fn run(self) {
//         log_with_timestamp("Starting UMCG HTTP server...");
//
//         // Start the executor
//         self.executor.start();
//
//         // Create worker pool first
//         for i in 0..4 {
//             let worker_id = i;
//             self.executor.submit(Box::new(move |handle: &TaskHandle| {
//                 log_with_timestamp(&format!("HTTP Worker {}: Ready for tasks", worker_id));
//                 loop {
//                     // Signal readiness for more work
//                     handle.submit(move |_| {
//                         log_with_timestamp(&format!("HTTP Worker {}: Signaling ready", worker_id));
//                     });
//                     std::thread::sleep(std::time::Duration::from_millis(100));
//                 }
//             }));
//         }
//
//         // Create acceptor task
//         let server = Arc::new(self.server);
//         let server_clone = server.clone();
//
//         self.executor.submit(Box::new(move |handle: &TaskHandle| {
//             log_with_timestamp("Starting HTTP acceptor");
//
//             for mut request in server_clone.incoming_requests() {
//                 let method = request.method().to_string();
//                 let url = request.url().to_string();
//
//                 log_with_timestamp(&format!("Acceptor: received {} {}", method, url));
//
//                 // Submit request handling as a new task
//                 handle.submit(move |_| {
//                     log_with_timestamp(&format!("Handler: processing {} {}", method, url));
//
//                     // Read request body
//                     let mut body = Vec::new();
//                     if let Ok(size) = request.as_reader().read_to_end(&mut body) {
//                         // Create response
//                         let response = Response::from_string(format!(
//                             "Handled {} {} with {} bytes\n",
//                             method, url, size
//                         ));
//
//                         // Send response
//                         match request.respond(response) {
//                             Ok(_) => log_with_timestamp("Response sent successfully"),
//                             Err(e) => log_with_timestamp(&format!("Error sending response: {}", e))
//                         }
//                     }
//
//                     // Yield after handling request
//                     std::thread::sleep(std::time::Duration::from_millis(1));
//                 });
//
//                 // Small delay between accepting requests
//                 std::thread::sleep(std::time::Duration::from_millis(1));
//             }
//         }));
//
//         // Keep main thread alive
//         loop {
//             std::thread::sleep(std::time::Duration::from_secs(1));
//             log_with_timestamp("Main: Server running...");
//         }
//     }
// }
//
// pub fn run_http_server() -> i32 {
//     log_with_timestamp("Starting HTTP server on 0.0.0.0:8080...");
//
//     match UMCGHttpServer::new("0.0.0.0:8080") {
//         Ok(server) => {
//             server.run();
//             0
//         },
//         Err(e) => {
//             log_with_timestamp(&format!("Failed to create server: {}", e));
//             1
//         }
//     }
// }