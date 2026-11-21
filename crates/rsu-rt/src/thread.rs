//! Thread scheduling helpers. Everything is best effort: a user without
//! real-time privileges still gets a working game, just with more jitter.

use std::ffi::CString;

/// Try to move the calling thread to SCHED_FIFO at `priority` (1..=99).
/// Returns the errno on failure (EPERM when RLIMIT_RTPRIO is 0).
pub fn set_realtime(priority: i32) -> Result<(), i32> {
    let param = libc::sched_param { sched_priority: priority };
    // SAFETY: param is valid; pthread_self() is always a valid thread.
    let r = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
    if r == 0 {
        Ok(())
    } else {
        Err(r)
    }
}

pub fn set_name(name: &str) {
    if let Ok(c) = CString::new(name) {
        // SAFETY: c is a valid NUL-terminated string (kernel truncates to 15 bytes).
        unsafe { libc::pthread_setname_np(libc::pthread_self(), c.as_ptr()) };
    }
}
