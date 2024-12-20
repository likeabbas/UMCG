// use io_uring::{opcode, types, IoUring};
// use solicit::http::connection::{HttpConnection, HttpFrame, StreamState};
// use solicit::http::frame::{Frame, FrameIR, Headers, Settings, WindowUpdate};
// use solicit::http::transport::TransportStream;
// use solicit::http::session::{DefaultSessionState, HttpSession, Stream};
// use solicit::http::{HttpScheme, StreamId};
// use std::{
//     collections::HashMap,
//     io::{self, Error, ErrorKind, Read, Write},
//     net::{TcpListener, TcpStream},
//     os::unix::io::{AsRawFd, RawFd},
//     time::{Duration, Instant},
// };
//
// const QUEUE_DEPTH: u32 = 1024;
// const READ_BUF_SIZE: usize = 16_384;
// const MAX_CONNECTIONS: usize = 10000;
// const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
//
// // Wrapper to make TcpStream work with solicit's TransportStream
// struct IoUringTransport {
//     stream: TcpStream,
//     read_buf: Vec<u8>,
//     write_buf: Vec<u8>,
// }
//
// impl IoUringTransport {
//     fn new(stream: TcpStream) -> Self {
//         Self {
//             stream,
//             read_buf: Vec::with_capacity(READ_BUF_SIZE),
//             write_buf: Vec::new(),
//         }
//     }
// }
//
// impl TransportStream for IoUringTransport {
//     fn try_read(&mut self) -> io::Result<Option<Vec<u8>>> {
//         if self.read_buf.is_empty() {
//             return Ok(None);
//         }
//         let data = self.read_buf.clone();
//         self.read_buf.clear();
//         Ok(Some(data))
//     }
//
//     fn try_write(&mut self, data: &[u8]) -> io::Result<Option<usize>> {
//         self.write_buf.extend_from_slice(data);
//         Ok(Some(data.len()))
//     }
// }
//
// enum ConnectionState {
//     Handshaking {
//         transport: IoUringTransport,
//         bytes_read: usize,
//     },
//     Active {
//         session: HttpSession<IoUringTransport, DefaultSessionState>,
//         last_activity: Instant,
//     },
// }
//
// struct Http2Server {
//     ring: IoUring,
//     listener: TcpListener,
//     connections: HashMap<RawFd, ConnectionState>,
//     routes: HashMap<String, Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>>,
// }
//
// impl Http2Server {
//     fn new(addr: &str) -> io::Result<Self> {
//         let ring = IoUring::new(QUEUE_DEPTH)?;
//         let listener = TcpListener::bind(addr)?;
//         listener.set_nonblocking(true)?;
//
//         Ok(Self {
//             ring,
//             listener,
//             connections: HashMap::new(),
//             routes: HashMap::new(),
//         })
//     }
//
//     fn add_route<F>(&mut self, path: &str, handler: F)
//     where
//         F: Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static,
//     {
//         self.routes.insert(path.to_string(), Box::new(handler));
//     }
//
//     fn accept_connections(&mut self) -> io::Result<()> {
//         while self.connections.len() < MAX_CONNECTIONS {
//             match self.listener.accept() {
//                 Ok((stream, _)) => {
//                     stream.set_nonblocking(true)?;
//                     let fd = stream.as_raw_fd();
//
//                     let transport = IoUringTransport::new(stream);
//                     let state = ConnectionState::Handshaking {
//                         transport,
//                         bytes_read: 0,
//                     };
//
//                     self.connections.insert(fd, state);
//                     self.submit_read(fd)?;
//                 }
//                 Err(e) if e.kind() == ErrorKind::WouldBlock => break,
//                 Err(e) => return Err(e),
//             }
//         }
//         Ok(())
//     }
//
//     fn submit_read(&mut self, fd: RawFd) -> io::Result<()> {
//         if let Some(state) = self.connections.get_mut(&fd) {
//             match state {
//                 ConnectionState::Handshaking { transport, .. } => {
//                     let read_op = opcode::Read::new(
//                         types::Fd(fd),
//                         transport.read_buf.as_mut_ptr(),
//                         READ_BUF_SIZE as u32,
//                     )
//                         .build()
//                         .user_data(fd as u64);
//
//                     unsafe {
//                         self.ring.submission()
//                             .push(&read_op)
//                             .map_err(|_| Error::new(ErrorKind::Other, "submission queue full"))?;
//                     }
//                 }
//                 ConnectionState::Active { session, .. } => {
//                     let transport = session.get_transport_mut();
//                     let read_op = opcode::Read::new(
//                         types::Fd(fd),
//                         transport.read_buf.as_mut_ptr(),
//                         READ_BUF_SIZE as u32,
//                     )
//                         .build()
//                         .user_data(fd as u64);
//
//                     unsafe {
//                         self.ring.submission()
//                             .push(&read_op)
//                             .map_err(|_| Error::new(ErrorKind::Other, "submission queue full"))?;
//                     }
//                 }
//             }
//         }
//         Ok(())
//     }
//
//     fn submit_write(&mut self, fd: RawFd) -> io::Result<()> {
//         if let Some(state) = self.connections.get_mut(&fd) {
//             match state {
//                 ConnectionState::Handshaking { transport, .. } => {
//                     if !transport.write_buf.is_empty() {
//                         let write_op = opcode::Write::new(
//                             types::Fd(fd),
//                             transport.write_buf.as_ptr(),
//                             transport.write_buf.len() as u32,
//                         )
//                             .build()
//                             .user_data(fd as u64);
//
//                         unsafe {
//                             self.ring.submission()
//                                 .push(&write_op)
//                                 .map_err(|_| Error::new(ErrorKind::Other, "submission queue full"))?;
//                         }
//                     }
//                 }
//                 ConnectionState::Active { session, .. } => {
//                     let transport = session.get_transport_mut();
//                     if !transport.write_buf.is_empty() {
//                         let write_op = opcode::Write::new(
//                             types::Fd(fd),
//                             transport.write_buf.as_ptr(),
//                             transport.write_buf.len() as u32,
//                         )
//                             .build()
//                             .user_data(fd as u64);
//
//                         unsafe {
//                             self.ring.submission()
//                                 .push(&write_op)
//                                 .map_err(|_| Error::new(ErrorKind::Other, "submission queue full"))?;
//                         }
//                     }
//                 }
//             }
//         }
//         Ok(())
//     }
//
//     fn handle_completion(&mut self, fd: RawFd, result: i32) -> io::Result<()> {
//         if result < 0 {
//             self.connections.remove(&fd);
//             return Ok(());
//         }
//
//         let bytes = result as usize;
//
//         match self.connections.get_mut(&fd) {
//             Some(ConnectionState::Handshaking { transport, bytes_read }) => {
//                 *bytes_read += bytes;
//
//                 // Check for HTTP/2 preface
//                 if *bytes_read >= 24 {
//                     let preface = &transport.read_buf[..*bytes_read];
//                     if preface.starts_with(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n") {
//                         // Initialize HTTP/2 session
//                         let mut settings = Settings::default();
//                         settings.set_max_concurrent_streams(100);
//                         settings.set_initial_window_size(65535);
//
//                         let session = HttpSession::new(
//                             transport.clone(),
//                             DefaultSessionState::new(),
//                             settings,
//                         );
//
//                         let state = ConnectionState::Active {
//                             session,
//                             last_activity: Instant::now(),
//                         };
//
//                         self.connections.insert(fd, state);
//                         self.submit_read(fd)?;
//                     }
//                 } else {
//                     self.submit_read(fd)?;
//                 }
//             }
//             Some(ConnectionState::Active { session, last_activity }) => {
//                 *last_activity = Instant::now();
//
//                 // Process incoming frames
//                 while let Some(frame) = session.get_next_frame()? {
//                     match frame {
//                         HttpFrame::Headers(headers) => {
//                             self.handle_headers(session, headers)?;
//                         }
//                         HttpFrame::Data(data) => {
//                             self.handle_data(session, data)?;
//                         }
//                         HttpFrame::WindowUpdate(update) => {
//                             session.handle_window_update(update)?;
//                         }
//                         _ => {} // Handle other frame types as needed
//                     }
//                 }
//
//                 // Submit any pending writes
//                 self.submit_write(fd)?;
//
//                 // Continue reading
//                 self.submit_read(fd)?;
//             }
//             None => {}
//         }
//
//         Ok(())
//     }
//
//     fn handle_headers(
//         &mut self,
//         session: &mut HttpSession<IoUringTransport, DefaultSessionState>,
//         headers: Headers,
//     ) -> io::Result<()> {
//         let stream_id = headers.stream_id();
//
//         // Extract path from headers
//         if let Some(path) = headers.headers().iter().find(|(name, _)| name == ":path") {
//             if let Some(handler) = self.routes.get(path.1) {
//                 // Create response headers
//                 let mut response_headers = Headers::new(stream_id);
//                 response_headers.add(":status", "200");
//                 response_headers.add("content-type", "text/plain");
//
//                 // Send response headers
//                 session.send_headers(response_headers)?;
//
//                 // Process request and send response data
//                 let response_data = handler(&[]);
//                 session.send_data(stream_id, response_data, true)?;
//             } else {
//                 // Send 404 response
//                 let mut response_headers = Headers::new(stream_id);
//                 response_headers.add(":status", "404");
//                 session.send_headers(response_headers)?;
//                 session.send_data(stream_id, b"Not Found".to_vec(), true)?;
//             }
//         }
//
//         Ok(())
//     }
//
//     fn handle_data(
//         &mut self,
//         session: &mut HttpSession<IoUringTransport, DefaultSessionState>,
//         data: Vec<u8>,
//     ) -> io::Result<()> {
//         // Handle request body data if needed
//         Ok(())
//     }
//
//     pub fn run(&mut self) -> io::Result<()> {
//         println!("HTTP/2 server running with:");
//         println!("  Max connections: {}", MAX_CONNECTIONS);
//         println!("  Queue depth: {}", QUEUE_DEPTH);
//         println!("  Read buffer size: {}", READ_BUF_SIZE);
//         println!("  Connection timeout: {}s", CONNECTION_TIMEOUT.as_secs());
//
//         loop {
//             self.accept_connections()?;
//
//             // Clean up timed out connections
//             self.connections.retain(|_, state| {
//                 match state {
//                     ConnectionState::Active { last_activity, .. } => {
//                         last_activity.elapsed() <= CONNECTION_TIMEOUT
//                     }
//                     ConnectionState::Handshaking { .. } => true,
//                 }
//             });
//
//             match self.ring.submit_and_wait(1) {
//                 Ok(_) => {
//                     while let Some(cqe) = self.ring.completion().next() {
//                         let fd = cqe.user_data() as RawFd;
//                         self.handle_completion(fd, cqe.result())?;
//                     }
//                 }
//                 Err(ref e) if e.raw_os_error() == Some(libc::EAGAIN) => continue,
//                 Err(e) => return Err(e),
//             }
//         }
//     }
// }
//
// fn main() -> io::Result<()> {
//     let mut server = Http2Server::new("127.0.0.1:8080")?;
//
//     // Add example routes
//     server.add_route("/", |_| {
//         b"Welcome to HTTP/2 server!".to_vec()
//     });
//
//     server.add_route("/echo", |body| {
//         body.to_vec()
//     });
//
//     println!("HTTP/2 server listening on http://127.0.0.1:8080");
//     server.run()
// }