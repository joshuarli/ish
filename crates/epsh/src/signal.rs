use std::sync::atomic::{AtomicBool, Ordering};

use rustix::process::Signal;

const PENDING_SIGNAL_COUNT: usize = 64;

static PENDING_SIGNALS: [AtomicBool; PENDING_SIGNAL_COUNT] =
    [const { AtomicBool::new(false) }; PENDING_SIGNAL_COUNT];

pub fn name_to_signal(name: &str) -> Option<Signal> {
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

pub fn signal_to_name(signum: i32) -> Option<&'static str> {
    let sig = Signal::from_named_raw(signum)?;
    match sig {
        Signal::HUP => Some("HUP"),
        Signal::INT => Some("INT"),
        Signal::QUIT => Some("QUIT"),
        Signal::ILL => Some("ILL"),
        Signal::TRAP => Some("TRAP"),
        Signal::ABORT => Some("ABRT"),
        Signal::FPE => Some("FPE"),
        Signal::KILL => Some("KILL"),
        Signal::BUS => Some("BUS"),
        Signal::SEGV => Some("SEGV"),
        Signal::SYS => Some("SYS"),
        Signal::PIPE => Some("PIPE"),
        Signal::ALARM => Some("ALRM"),
        Signal::TERM => Some("TERM"),
        Signal::URG => Some("URG"),
        Signal::STOP => Some("STOP"),
        Signal::TSTP => Some("TSTP"),
        Signal::CONT => Some("CONT"),
        Signal::CHILD => Some("CHLD"),
        Signal::TTIN => Some("TTIN"),
        Signal::TTOU => Some("TTOU"),
        Signal::USR1 => Some("USR1"),
        Signal::USR2 => Some("USR2"),
        _ => None,
    }
}

// rustix 1.1.5 hides its runtime signal API, so disposition changes use the platform libc ABI.
pub fn install_handler(sig: Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    return linux_sigaction(
        sig,
        signal_handler as *const () as usize,
        libc::SA_RESTART,
    );
    #[cfg(target_os = "macos")]
    return darwin_sigaction(sig, signal_handler as *const () as usize, 0x0002);
}

pub fn reset_handler(sig: Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    return linux_sigaction(sig, libc::SIG_DFL, 0);
    #[cfg(target_os = "macos")]
    return darwin_sigaction(sig, 0, 0);
}

pub fn ignore_signal(sig: Signal) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    return linux_sigaction(sig, libc::SIG_IGN, 0);
    #[cfg(target_os = "macos")]
    return darwin_sigaction(sig, 1, 0);
}

pub fn take_pending() -> Vec<&'static str> {
    PENDING_SIGNALS
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(signum, pending)| {
            if pending.swap(false, Ordering::Relaxed) {
                signal_to_name(signum as i32)
            } else {
                None
            }
        })
        .collect()
}

extern "C" fn signal_handler(signum: i32) {
    if let Ok(signum) = usize::try_from(signum)
        && let Some(pending) = PENDING_SIGNALS.get(signum)
    {
        pending.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn linux_sigaction(
    signal: Signal,
    handler: usize,
    flags: core::ffi::c_int,
) -> std::io::Result<()> {
    // SAFETY: zero initializes the libc action fields before the mask is initialized below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    action.sa_flags = flags;
    // SAFETY: the mask is a valid mutable libc sigset_t.
    let empty_set = unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if empty_set != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the signal and handler use libc's Linux ABI, and sigaction copies the initialized action.
    let result = unsafe { libc::sigaction(signal.as_raw(), &action, std::ptr::null_mut()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct DarwinSigaction {
    sa_sigaction: usize,
    sa_mask: u32,
    sa_flags: core::ffi::c_int,
}

#[cfg(target_os = "macos")]
fn darwin_sigaction(
    signal: Signal,
    handler: usize,
    flags: core::ffi::c_int,
) -> std::io::Result<()> {
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
    // SAFETY: the action has the Darwin ABI layout and the API copies it before returning.
    let result = unsafe { sigaction(signal.as_raw(), &action, std::ptr::null_mut()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
