use std::time::SystemTime;
use libc::{self, pid_t, syscall, SYS_gettid, EINTR};

pub static DEBUG_LOGGING: bool = true;

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

fn debug_worker_id(worker_id: u64) -> String {
    let server_id = (worker_id >> 32) as u32;
    let tid = (worker_id & 0xFFFFFFFF) as u32;
    format!("server:{} tid:{} (raw:{})", server_id, tid, worker_id)
}

pub const SYS_UMCG_CTL: i64 = 449;
pub const UMCG_WORKER_ID_SHIFT: u64 = 5;
pub const UMCG_WORKER_EVENT_MASK: u64 = (1 << UMCG_WORKER_ID_SHIFT) - 1;
pub const UMCG_WAIT_FLAG_INTERRUPTED: u64 = 1;
pub const WORKER_REGISTRATION_TIMEOUT_MS: u64 = 5000;
// 5 second timeout for worker registration
pub const EVENT_BUFFER_SIZE: usize = 2;

#[derive(Debug, Copy, Clone)]
#[repr(u64)]
pub enum UmcgCmd {
    RegisterWorker = 1,
    RegisterServer,
    Unregister,
    Wake,
    Wait,
    CtxSwitch,
}

#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u64)]
pub enum UmcgEventType {
    Block = 1,
    Wake,
    Wait,
    Exit,
    Timeout,
    Preempt,
}

impl UmcgEventType {
    pub fn from_u64(value: u64) -> Option<UmcgEventType> {
        match value {
            1 => Some(UmcgEventType::Block),
            2 => Some(UmcgEventType::Wake),
            3 => Some(UmcgEventType::Wait),
            4 => Some(UmcgEventType::Exit),
            5 => Some(UmcgEventType::Timeout),
            6 => Some(UmcgEventType::Preempt),
            _ => None
        }
    }
}

pub fn get_thread_id() -> pid_t {
    unsafe { syscall(SYS_gettid) as pid_t }
}

pub fn set_cpu_affinity(cpu_id: usize) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        let set_ref = &mut *set.as_mut_ptr();

        libc::CPU_ZERO(set_ref);
        libc::CPU_SET(cpu_id, set_ref);

        let result = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set.as_ptr());
        if result != 0 {
            debug!("Could not set CPU affinity to {}: {}", cpu_id, std::io::Error::last_os_error());
        } else {
            debug!("Successfully set CPU affinity to {}", cpu_id);
        }
    }
    Ok(())
}

pub fn sys_umcg_ctl(
    flags: u64,
    cmd: UmcgCmd,
    next_tid: pid_t,
    abs_timeout: u64,
    events: Option<&mut [u64]>,
    event_sz: i32,
) -> i32 {
    // debug!("UMCG syscall - cmd: {:?}, tid: {}, flags: {}", cmd, next_tid, flags);
    let result = unsafe {
        syscall(
            SYS_UMCG_CTL,
            flags as i64,
            cmd as i64,
            next_tid as i64,
            abs_timeout as i64,
            events.map_or(std::ptr::null_mut(), |e| e.as_mut_ptr()) as i64,
            event_sz as i64,
        ) as i32
    };
    // debug!("UMCG syscall result: {}", result);
    result
}

pub fn umcg_wait_retry(worker_id: u64, mut events_buf: Option<&mut [u64]>, event_sz: i32) -> i32 {
    let mut flags = 0;
    loop {
        debug!("!!!!!!!!!! UMCG WAIT RETRY START - worker: {}, flags: {} !!!!!!!!!!", worker_id, flags);
        let events = events_buf.as_deref_mut();
        let ret = sys_umcg_ctl(
            flags,
            UmcgCmd::Wait,
            (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
            0,
            events,
            event_sz,
        );
        debug!("!!!!!!!!!! UMCG WAIT RETRY RETURNED: {} !!!!!!!!!!", ret);
        if ret != -1 || unsafe { *libc::__errno_location() } != EINTR {
            return ret;
        }
        flags = UMCG_WAIT_FLAG_INTERRUPTED;
    }
}

#[derive(Debug)]
pub enum WaitNoTimeoutResult {
    Events(i32),          // Got events, here's the syscall result
    ProcessForwarded,     // Check forwarded events
    ResourceBusy,         // Got EAGAIN, should back off
}

pub fn umcg_wait_retry_simple(
    worker_id: u64,
    mut events_buf: Option<&mut [u64]>,
    event_sz: i32,
) -> WaitNoTimeoutResult {
    let flags = 0;

    let events = events_buf.as_deref_mut();
    let ret = sys_umcg_ctl(
        flags,
        UmcgCmd::Wait,
        (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
        0, // No timeout
        events,
        event_sz,
    );

    let errno = unsafe { *libc::__errno_location() };

    if ret == 0 {
        return WaitNoTimeoutResult::Events(ret);
    }

    match errno {
        libc::EAGAIN => WaitNoTimeoutResult::ResourceBusy,
        libc::EINTR => WaitNoTimeoutResult::ProcessForwarded,
        _ => WaitNoTimeoutResult::Events(ret)
    }
}

#[derive(Debug)]
pub enum WaitResult {
    Events(i32),          // Got events, here's the syscall result
    ProcessForwarded,     // Check forwarded events
    Timeout,              // Regular timeout
    ResourceBusy,         // Got EAGAIN, should back off
}

pub fn umcg_wait_retry_timeout(
    worker_id: u64,
    mut events_buf: Option<&mut [u64]>,
    event_sz: i32,
    timeout_ns: u64,
    // debug_count: u64,  // Current count
    // debug_hash: u64,   // Print debug if count % hash == 0
) -> WaitResult {
    let mut flags = 0;
    // let should_debug = debug_count % debug_hash == 0;

    // Calculate absolute timeout
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let abs_timeout = now.as_nanos() as u64 + timeout_ns;

    // if should_debug {
    //     debug!("!!!!!!!!!! UMCG WAIT RETRY START - worker: {}, flags: {}, timeout: {} ns !!!!!!!!!!",
    //         worker_id, flags, timeout_ns);
    // }

    let events = events_buf.as_deref_mut();
    let ret = sys_umcg_ctl(
        flags,
        UmcgCmd::Wait,
        (worker_id >> UMCG_WORKER_ID_SHIFT) as pid_t,
        abs_timeout,
        events,
        event_sz,
    );

    // if should_debug {
    //     debug!("!!!!!!!!!! UMCG WAIT RETRY RETURNED: {} !!!!!!!!!!", ret);
    // }

    let errno = unsafe { *libc::__errno_location() };

    if ret == 0 {
        return WaitResult::Events(ret);
    }

    match errno {
        libc::ETIMEDOUT => WaitResult::Timeout,
        libc::EAGAIN => WaitResult::ResourceBusy,
        libc::EINTR => {
            if errno != libc::ETIMEDOUT {
                flags = UMCG_WAIT_FLAG_INTERRUPTED;
                WaitResult::ProcessForwarded
            } else {
                WaitResult::Timeout
            }
        }
        _ => WaitResult::Events(ret)
    }
}