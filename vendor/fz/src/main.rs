// Rust's runtime start-up (SIGPIPE override, stack-guard handler install, main
// thread bookkeeping) costs about half a millisecond per process, which is most
// of what separates fz from a C binary on small inputs. fz needs none of it:
// SIGPIPE keeps its default disposition as in fzy, and `run` flushes its own
// output because nothing flushes stdout at exit.
#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
use std::ffi::{c_int, c_char, CStr};

#[cfg(not(test))]
#[unsafe(no_mangle)]
/// Process entry point; arguments arrive via `argc`/`argv` because musl
/// populates `std::env::args` only during Rust runtime start-up, which this
/// entry point skips; they would arrive empty.
///
/// # Safety
///
/// `argv` must hold `argc` non-null pointers to NUL-terminated C strings.
pub unsafe extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args = (1..usize::try_from(argc).unwrap_or(0))
        // SAFETY: the process entry contract is that `argv` holds `argc` non-null C strings, and this range stays below `argc`.
        .map(|index| unsafe { CStr::from_ptr(*argv.add(index)) }.to_bytes().to_vec());
    let status = fz::cli::run(args);
    if matches!(status - 128, libc::SIGHUP | libc::SIGTERM | libc::SIGQUIT) {
        // SAFETY: the terminal adapter has restored the caller's state. The
        // executable re-raises handled termination with its default disposition.
        unsafe { libc::signal(status - 128, libc::SIG_DFL); libc::raise(status - 128); }
    }
    status
}
