use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

static DEBUG_LOGGING: bool = true;

macro_rules! info {
    ($($arg:tt)*) => {{
        log_with_timestamp(&format!($($arg)*));
    }};
}

macro_rules! debug {
    ($($arg:tt)*) => {{
        if DEBUG_LOGGING {
            log_with_timestamp(&format!($($arg)*));
        }
    }};
}

fn log_with_timestamp(msg: &str) {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    println!("[{:>3}.{:03}] {}",
             now.as_secs() % 1000,
             now.subsec_millis(),
             msg);
}

#[derive(Default)]
pub struct TaskStats {
    pub completed_tasks: Arc<Mutex<HashMap<Uuid, bool>>>,
    pub total_tasks: AtomicUsize,
    pub completed_count: AtomicUsize,
}

impl TaskStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            completed_tasks: Arc::new(Mutex::new(HashMap::new())),
            total_tasks: AtomicUsize::new(0),
            completed_count: AtomicUsize::new(0),
        })
    }

    pub fn register_task(&self, task_id: Uuid) {
        let mut tasks = self.completed_tasks.lock().unwrap();
        tasks.insert(task_id, false);
        self.total_tasks.fetch_add(1, Ordering::SeqCst);
        debug!("Registered task {}, total tasks: {}", task_id, self.total_tasks.load(Ordering::SeqCst));
    }

    pub fn mark_completed(&self, task_id: Uuid) {
        let mut tasks = self.completed_tasks.lock().unwrap();
        if let Some(completed) = tasks.get_mut(&task_id) {
            if !*completed {
                *completed = true;
                self.completed_count.fetch_add(1, Ordering::SeqCst);
                debug!("Completed task {}, total completed: {}/{}",
                    task_id,
                    self.completed_count.load(Ordering::SeqCst),
                    self.total_tasks.load(Ordering::SeqCst));
            }
        }
    }

    pub fn all_tasks_completed(&self) -> bool {
        let completed = self.completed_count.load(Ordering::SeqCst);
        let total = self.total_tasks.load(Ordering::SeqCst);
        completed == total && total > 0
    }
}