use std::sync::atomic::{AtomicI32, Ordering};

static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING.store(sig, Ordering::SeqCst);
}

/// Records SIGINT and SIGTERM so the caller can forward them to a scope.
pub fn install_termination_handler() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(sig, &act, std::ptr::null_mut());
        }
    }
}

/// The last signal received since the previous call, if any.
pub fn take_pending() -> Option<i32> {
    match PENDING.swap(0, Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}
