//! Signal handling for trap execution.
//!
//! When a trap is set for a signal (e.g. `trap 'cleanup' INT`), we install a
//! signal handler that sets a global atomic flag. The shell checks these flags
//! between commands and runs the corresponding trap action.
//!
//! Signal numbers are rustix `Signal` values (no libc constants). Handler
//! installation registers a raw C-style handler pointer via `sigaction`
//! (avoiding rustix's `kernel_sigaction` trampoline, which is unsafe under
//! static-musl linking); reset/ignore use rustix `kernel_sigaction` with the
//! `SIG_DFL`/`SIG_IGN` kernel handlers (no trampoline involved).

use std::sync::atomic::{AtomicBool, Ordering};

use rustix::process::Signal;

static SIGINT_PENDING: AtomicBool = AtomicBool::new(false);
static SIGTERM_PENDING: AtomicBool = AtomicBool::new(false);
static SIGHUP_PENDING: AtomicBool = AtomicBool::new(false);

/// Map a signal name (e.g. "INT", "SIGINT") to its `Signal`.
pub fn name_to_signal(name: &str) -> Option<Signal> {
    // Strip "SIG" prefix if present
    let name = name.strip_prefix("SIG").unwrap_or(name);
    match name {
        "HUP" => Some(Signal::HUP),
        "INT" => Some(Signal::INT),
        "QUIT" => Some(Signal::QUIT),
        "ILL" => Some(Signal::ILL),
        "TRAP" => Some(Signal::TRAP),
        "ABRT" | "IOT" => Some(Signal::ABORT),
        "FPE" => Some(Signal::FPE),
        "KILL" => Some(Signal::KILL),
        "BUS" => Some(Signal::BUS),
        "SEGV" => Some(Signal::SEGV),
        "SYS" => Some(Signal::SYS),
        "PIPE" => Some(Signal::PIPE),
        "ALRM" => Some(Signal::ALARM),
        "TERM" => Some(Signal::TERM),
        "URG" => Some(Signal::URG),
        "STOP" => Some(Signal::STOP),
        "TSTP" => Some(Signal::TSTP),
        "CONT" => Some(Signal::CONT),
        "CHLD" => Some(Signal::CHILD),
        "TTIN" => Some(Signal::TTIN),
        "TTOU" => Some(Signal::TTOU),
        "USR1" => Some(Signal::USR1),
        "USR2" => Some(Signal::USR2),
        _ => None,
    }
}

/// Map a signal number to its name (without SIG prefix).
pub fn signal_to_name(signum: i32) -> Option<&'static str> {
    let sig = Signal::from_named_raw(signum)?;
    let name = match sig {
        Signal::HUP => "HUP",
        Signal::INT => "INT",
        Signal::QUIT => "QUIT",
        Signal::ILL => "ILL",
        Signal::TRAP => "TRAP",
        Signal::ABORT => "ABRT",
        Signal::FPE => "FPE",
        Signal::KILL => "KILL",
        Signal::BUS => "BUS",
        Signal::SEGV => "SEGV",
        Signal::SYS => "SYS",
        Signal::PIPE => "PIPE",
        Signal::ALARM => "ALRM",
        Signal::TERM => "TERM",
        Signal::URG => "URG",
        Signal::STOP => "STOP",
        Signal::TSTP => "TSTP",
        Signal::CONT => "CONT",
        Signal::CHILD => "CHLD",
        Signal::TTIN => "TTIN",
        Signal::TTOU => "TTOU",
        Signal::USR1 => "USR1",
        Signal::USR2 => "USR2",
        _ => return None,
    };
    Some(name)
}

/// Install a signal handler that sets the pending flag for the given signal.
/// Call this when a trap is set for a signal.
pub fn install_handler(sig: Signal) {
    // SAFETY: sigaction is async-signal-safe. The handler only sets an atomic
    // flag. We register a raw C-style handler pointer (SA_RESTART) rather than
    // rustix's trampoline-based kernel_sigaction, which is unsafe under static
    // musl linking.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = signal_handler as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig.as_raw(), &sa, std::ptr::null_mut());
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), signal_handler as *const () as usize, 0x0002);
        }
    }
}

/// Reset a signal to its default disposition.
/// Call this when a trap is removed for a signal.
pub fn reset_handler(sig: Signal) {
    // SAFETY: kernel_sigaction with KERNEL_SIG_DFL is always safe.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let action = rustix::runtime::KernelSigaction {
                sa_handler_kernel: rustix::runtime::KERNEL_SIG_DFL,
                sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
                sa_restorer: None,
                sa_mask: rustix::runtime::KernelSigSet::empty(),
            };
            let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), 0, 0);
        }
    }
}

/// Set a signal to be ignored (for `trap '' SIG`).
pub fn ignore_signal(sig: Signal) {
    // SAFETY: kernel_sigaction with kernel_sig_ign() is always safe.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            let action = rustix::runtime::KernelSigaction {
                sa_handler_kernel: rustix::runtime::kernel_sig_ign(),
                sa_flags: rustix::runtime::KernelSigactionFlags::empty(),
                sa_restorer: None,
                sa_mask: rustix::runtime::KernelSigSet::empty(),
            };
            let _ = rustix::runtime::kernel_sigaction(sig, Some(action));
        }
        #[cfg(target_os = "macos")]
        {
            darwin_sigaction(sig.as_raw(), 1, 0);
        }
    }
}

/// Check and clear all pending signal flags. Returns signal names that were pending.
pub fn take_pending() -> Vec<&'static str> {
    let mut pending = Vec::new();
    if SIGINT_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("INT");
    }
    if SIGTERM_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("TERM");
    }
    if SIGHUP_PENDING.swap(false, Ordering::Relaxed) {
        pending.push("HUP");
    }
    pending
}

extern "C" fn signal_handler(signum: i32) {
    match signum {
        x if x == Signal::INT.as_raw() => SIGINT_PENDING.store(true, Ordering::Relaxed),
        x if x == Signal::TERM.as_raw() => SIGTERM_PENDING.store(true, Ordering::Relaxed),
        x if x == Signal::HUP.as_raw() => SIGHUP_PENDING.store(true, Ordering::Relaxed),
        _ => {}
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinSigaction {
    sa_sigaction: usize,
    sa_mask: u32,
    sa_flags: core::ffi::c_int,
}

#[cfg(target_os = "macos")]
unsafe fn darwin_sigaction(signal: i32, handler: usize, flags: core::ffi::c_int) {
    unsafe extern "C" {
        fn sigaction(
            signal: core::ffi::c_int,
            action: *const DarwinSigaction,
            old_action: *mut DarwinSigaction,
        ) -> core::ffi::c_int;
    }
    let action = DarwinSigaction {
        sa_sigaction: handler,
        sa_mask: 0,
        sa_flags: flags,
    };
    let _ = unsafe { sigaction(signal, &action, std::ptr::null_mut()) };
}
