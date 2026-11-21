//! CLOCK_MONOTONIC is the one clock domain every timestamp lives in: evdev
//! events (EVIOCSCLOCKID), PipeWire cycle times and our own frame marks.

#[inline]
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out pointer; the vDSO call cannot fail for this clock.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Sleep until an absolute CLOCK_MONOTONIC time. Returns immediately if it
/// has already passed.
pub fn sleep_until_ns(deadline: u64) {
    let ts = libc::timespec {
        tv_sec: (deadline / 1_000_000_000) as libc::time_t,
        tv_nsec: (deadline % 1_000_000_000) as libc::c_long,
    };
    loop {
        // SAFETY: ts is valid; a null remainder is allowed with TIMER_ABSTIME.
        let r = unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
        if r != libc::EINTR {
            return;
        }
    }
}

/// Sleep for most of the wait and spin for the last `spin_ns`, trading a
/// little CPU for a wake-up that is not at the mercy of the timer slack.
pub fn sleep_until_precise(deadline: u64, spin_ns: u64) {
    let now = now_ns();
    if deadline > now + spin_ns {
        sleep_until_ns(deadline - spin_ns);
    }
    while now_ns() < deadline {
        std::hint::spin_loop();
    }
}
