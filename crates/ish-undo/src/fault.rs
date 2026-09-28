//! Deterministic fault injection for tests.
//!
//! The shell never installs a plan, so every check is a single relaxed atomic
//! load. Tests install a plan in a dedicated fixture process to fail or abort
//! at a named boundary (for example the third `unlink`, or right after a
//! journal prepare record). Nothing here is reachable from shell input.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PLAN: Mutex<Vec<Fault>> = Mutex::new(Vec::new());

#[derive(Clone, Debug)]
pub enum FaultAction {
    /// Fail the operation with this errno.
    Errno(i32),
    /// Terminate the process immediately, simulating a crash.
    Abort,
}

#[derive(Clone, Debug)]
struct Fault {
    point: String,
    /// Number of matching checks to let through before firing.
    skip: u32,
    action: FaultAction,
}

/// Install a fault: after `skip` passing checks of `point`, perform `action`.
#[doc(hidden)]
pub fn inject(point: &str, skip: u32, action: FaultAction) {
    PLAN.lock().unwrap().push(Fault {
        point: point.to_owned(),
        skip,
        action,
    });
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Parse `point[:skip]=errno|abort` specifications, as used by the fixture.
#[doc(hidden)]
pub fn inject_spec(spec: &str) -> Result<(), String> {
    let (lhs, action) = spec.split_once('=').ok_or("expected point=action")?;
    let (point, skip) = match lhs.split_once(':') {
        Some((p, n)) => (p, n.parse::<u32>().map_err(|e| e.to_string())?),
        None => (lhs, 0),
    };
    let action = match action {
        "abort" => FaultAction::Abort,
        "ENOSPC" => FaultAction::Errno(libc::ENOSPC),
        "EXDEV" => FaultAction::Errno(libc::EXDEV),
        "EIO" => FaultAction::Errno(libc::EIO),
        "EACCES" => FaultAction::Errno(libc::EACCES),
        "ENOTSUP" => FaultAction::Errno(libc::ENOTSUP),
        other => FaultAction::Errno(other.parse::<i32>().map_err(|e| e.to_string())?),
    };
    inject(point, skip, action);
    Ok(())
}

/// Check a named boundary.
#[inline]
pub fn check(point: &str) -> io::Result<()> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return Ok(());
    }
    fire(point)
}

#[cold]
fn fire(point: &str) -> io::Result<()> {
    let mut plan = PLAN.lock().unwrap();
    let Some(index) = plan.iter().position(|f| f.point == point) else {
        return Ok(());
    };
    if plan[index].skip > 0 {
        plan[index].skip -= 1;
        return Ok(());
    }
    let fault = plan.remove(index);
    drop(plan);
    match fault.action {
        FaultAction::Errno(code) => Err(io::Error::from_raw_os_error(code)),
        FaultAction::Abort => std::process::abort(),
    }
}
