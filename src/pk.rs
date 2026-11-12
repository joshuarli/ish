use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use crate::input::{InputEvent, InputReader, Key};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux::Native;
#[cfg(target_os = "macos")]
use macos::Native;

pub const HELP: &str = "Usage: pk [-9|--force] [-p|--port PORT] [--] [query...]\nSelect your processes and send TERM (or KILL with -9). A query or TCP port is required.\nDown enters results; Space marks; Enter accepts; Escape or Ctrl+C cancels.\n";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Select { signal: rustix::process::Signal, port: Option<u16>, query: Vec<u8> },
}

fn parse(args: &[OsString]) -> Result<Command, &'static str> {
    let mut signal = rustix::process::Signal::TERM;
    let mut port = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_bytes() {
            b"-h" | b"--help" => return Ok(Command::Help),
            b"-9" | b"--force" => signal = rustix::process::Signal::KILL,
            b"-p" | b"--port" => {
                i += 1;
                let bytes = args.get(i).ok_or("--port requires a decimal port from 1 to 65535")?.as_bytes();
                if bytes.is_empty() { return Err("port must be decimal from 1 to 65535"); }
                let mut value = 0u16;
                for &byte in bytes {
                    if !byte.is_ascii_digit() { return Err("port must be decimal from 1 to 65535"); }
                    value = value.checked_mul(10).and_then(|v| v.checked_add(u16::from(byte - b'0')))
                        .ok_or("port must be decimal from 1 to 65535")?;
                }
                if value == 0 { return Err("port must be decimal from 1 to 65535"); }
                port = Some(value);
            }
            b"--" => { i += 1; break; }
            arg if arg.starts_with(b"-") => return Err("unknown option; use -- before an option-looking query"),
            _ => break,
        }
        i += 1;
    }
    if i == args.len() && port.is_none() { return Err("provide a process query or --port PORT"); }
    let mut query = Vec::new();
    for (index, arg) in args[i..].iter().enumerate() {
        if index != 0 { query.push(b' '); }
        query.extend_from_slice(arg.as_bytes());
    }
    Ok(Command::Select { signal, port, query })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Pid(std::num::NonZeroI32);
impl Pid {
    fn new(value: i32) -> io::Result<Self> {
        if value <= 1 { return Err(io::Error::other("invalid process PID")); }
        Ok(Self(std::num::NonZeroI32::new(value).unwrap()))
    }
    fn raw(self) -> i32 { self.0.get() }
    fn native(self) -> rustix::process::Pid { rustix::process::Pid::from_raw(self.raw()).unwrap() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Metadata {
    pid: Pid,
    parent: i32,
    real_uid: u32,
    effective_uid: u32,
    started: (u64, u64),
    executable: Vec<u8>,
    argv: Vec<u8>,
}

trait Backend {
    type Handle;
    fn list(&self) -> io::Result<Vec<Pid>>;
    fn metadata(&self, pid: Pid) -> io::Result<Metadata>;
    fn ancestry(&self, pid: Pid) -> io::Result<i32> { self.metadata(pid).map(|metadata| metadata.parent) }
    fn bind(&self, record: &Metadata) -> io::Result<Self::Handle>;
    fn listens(&self, pid: Pid, port: u16) -> io::Result<bool>;
    fn signal(&self, handle: &Self::Handle, signal: rustix::process::Signal) -> io::Result<()>;
}

struct Record<H> { metadata: Metadata, handle: H }
struct Snapshot<H> {
    records: Vec<Record<H>>,
    protected: HashSet<Pid>,
    uid: u32,
    port: Option<u16>,
    inaccessible: usize,
}

fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn snapshot<B: Backend>(backend: &B, current: Pid, uid: u32, port: Option<u16>) -> io::Result<Snapshot<B::Handle>> {
    let mut protected = HashSet::new();
    let mut next = current;
    loop {
        if !protected.insert(next) { return Err(io::Error::other("cycle in invoking process ancestry")); }
        let parent = backend.ancestry(next)?;
        if parent <= 1 { break; }
        next = Pid::new(parent)?;
        if protected.len() >= 1024 { return Err(io::Error::other("invoking process ancestry exceeds limit")); }
    }
    let mut result = Snapshot { records: Vec::new(), protected, uid, port, inaccessible: 0 };
    let mut seen = HashSet::new();
    for pid in backend.list()? {
        if result.protected.contains(&pid) || !seen.insert(pid) { continue; }
        let record = match backend.metadata(pid) {
            Ok(record) if record.effective_uid == uid => record,
            Ok(_) => continue,
            Err(error) if gone(&error) => continue,
            Err(_) => { result.inaccessible += 1; continue; }
        };
        if let Some(port) = port {
            match backend.listens(pid, port) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) if gone(&error) => continue,
                Err(_) => { result.inaccessible += 1; continue; }
            }
        }
        if result.records.len() == 4096 { return Err(io::Error::other("more than 4096 eligible processes; narrow with --port")); }
        let handle = match backend.bind(&record) {
            Ok(handle) => handle,
            Err(error) if gone(&error) => continue,
            Err(error) => return Err(error),
        };
        result.records.push(Record { metadata: record, handle });
    }
    if result.records.is_empty() && result.inaccessible != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("no selectable metadata available; {} processes could not be inspected", result.inaccessible)));
    }
    result.records.sort_unstable_by_key(|record| record.metadata.pid.raw());
    Ok(result)
}

struct Batch {
    sent: Vec<Pid>,
    failed: Vec<(Pid, io::Error)>,
    termination: Option<i32>,
}

fn send<B: Backend>(backend: &B, snapshot: &Snapshot<B::Handle>, ids: &[usize], signal: rustix::process::Signal, mut interrupted: impl FnMut() -> Option<i32>) -> io::Result<Batch> {
    let mut unique_ids = HashSet::new();
    let mut pids = HashSet::new();
    for &id in ids {
        let record = snapshot.records.get(id).ok_or_else(|| io::Error::other("selection is outside the process snapshot"))?;
        let meta = &record.metadata;
        if !unique_ids.insert(id) || !pids.insert(meta.pid) || meta.started == (0, 0)
            || meta.pid.raw() <= 1 || snapshot.protected.contains(&meta.pid) || meta.effective_uid != snapshot.uid
        { return Err(io::Error::other("invalid, duplicate or protected process selection")); }
    }
    let mut result = Batch { sent: Vec::new(), failed: Vec::new(), termination: None };
    for &id in ids {
        if let Some(signal) = interrupted() { result.termination = Some(signal); break; }
        let record = &snapshot.records[id];
        let pid = record.metadata.pid;
        let attempt = (|| {
            let current = backend.metadata(pid)?;
            if current != record.metadata { return Err(io::Error::other("process identity, ownership, parent or command changed")); }
            if let Some(port) = snapshot.port && !backend.listens(pid, port)? {
                return Err(io::Error::other("process no longer owns an eligible TCP listener"));
            }
            if backend.metadata(pid)? != record.metadata { return Err(io::Error::other("process changed during listener revalidation")); }
            if let Some(signal) = interrupted() { result.termination = Some(signal); return Ok(false); }
            backend.signal(&record.handle, signal)?;
            Ok(true)
        })();
        match attempt {
            Ok(true) => result.sent.push(pid),
            Ok(false) => break,
            Err(error) => result.failed.push((pid, error)),
        }
    }
    Ok(result)
}

fn display_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\0' { output.push(b' '); }
            else if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                write!(output, "{}", c.escape_default()).unwrap();
            } else {
                let mut buffer = [0; 4];
                output.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
        }
        for byte in chunk.invalid() { write!(output, "\\x{byte:02x}").unwrap(); }
    }
    output
}

pub enum RunOutcome { Completed(i32), Terminated(i32) }

pub fn run(args: &[OsString], reader: &mut InputReader) -> RunOutcome {
    let (signal, port, query) = match parse(args) {
        Ok(Command::Help) => { print!("{HELP}"); return RunOutcome::Completed(0); }
        Ok(Command::Select { signal, port, query }) => (signal, port, query),
        Err(error) => { eprintln!("pk: {error}\n{HELP}"); return RunOutcome::Completed(2); }
    };
    let backend = Native;
    let current = match Pid::new(rustix::process::getpid().as_raw_pid()) {
        Ok(current) => current,
        Err(error) => { eprintln!("pk: cannot establish invoking process protection: {error}"); return RunOutcome::Completed(1); }
    };
    let snapshot = match snapshot(&backend, current, rustix::process::geteuid().as_raw(), port) {
        Ok(snapshot) => snapshot,
        Err(error) => { eprintln!("pk: process lookup failed: {error}"); return RunOutcome::Completed(1); }
    };
    if snapshot.inaccessible != 0 { eprintln!("pk: {} processes could not be inspected", snapshot.inaccessible); }
    if snapshot.records.is_empty() {
        if port.is_some() { eprintln!("pk: no eligible TCP listener found; some processes may be inaccessible"); return RunOutcome::Completed(1); }
        return RunOutcome::Completed(0);
    }
    let rows = snapshot.records.iter().map(|record| {
        let m = &record.metadata;
        let mut row = format!("{} ", m.pid.raw()).into_bytes();
        row.extend_from_slice(&display_bytes(m.executable.rsplit(|&byte| byte == b'/').next().unwrap_or(&m.executable)));
        row.extend_from_slice(b"  ");
        row.extend_from_slice(&display_bytes(&m.argv));
        row
    }).collect();
    let mut picker = fz::Picker::new(fz::Candidates::from_records(rows), fz::PickerConfig {
        lines: 10, prompt: if signal == rustix::process::Signal::KILL { b"KILL> " } else { b"TERM> " }.to_vec(),
        query: display_bytes(&query), show_scores: false, show_info: true, multi: true, workers: 1,
    });
    let outcome = pick(&mut picker, reader);
    let ids = match outcome {
        Ok(PickOutcome::Selected(fz::Outcome::Accepted(ids))) => ids.iter().map(|id| id.index()).collect::<Vec<_>>(),
        Ok(PickOutcome::Selected(_)) => return RunOutcome::Completed(0),
        Ok(PickOutcome::Terminated(sig)) => return RunOutcome::Terminated(sig),
        Err(error) => { eprintln!("pk: picker failed: {error}"); return RunOutcome::Completed(2); }
    };
    match send(&backend, &snapshot, &ids, signal, crate::signal::take_pending_termination) {
        Ok(batch) => {
            for pid in batch.sent { eprintln!("pk: {}: {} signal sent", pid.raw(), if signal == rustix::process::Signal::KILL { "KILL" } else { "TERM" }); }
            for (pid, error) in &batch.failed { eprintln!("pk: {}: skipped: {error}", pid.raw()); }
            if let Some(signal) = batch.termination { RunOutcome::Terminated(signal) }
            else { RunOutcome::Completed(i32::from(!batch.failed.is_empty())) }
        }
        Err(error) => { eprintln!("pk: {error}; no signals sent"); RunOutcome::Completed(1) }
    }
}

enum PickOutcome { Selected(fz::Outcome), Terminated(i32) }

fn pick(picker: &mut fz::Picker, reader: &mut InputReader) -> io::Result<PickOutcome> {
    let _signals = crate::signal::ForegroundSignals::install()?;
    let original = rustix::termios::tcgetattr(crate::term::stdin())?;
    let mut raw = original.clone();
    raw.make_raw();
    raw.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 0;
    raw.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;
    rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Now, &raw)?;
    struct Restore(crate::term::Termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Now, &self.0);
            let _ = io::stdout().write_all(b"\x1b[?2004l\x1b[0m\x1b[?25h");
        }
    }
    let restore = Restore(original);
    let result = (|| {
        let mut output = io::stdout().lock();
        output.write_all(b"\x1b[?2004h")?;
        loop {
            let (rows, cols) = crate::term::term_size();
            if rows < 4 || cols < 20 { return Err(io::Error::other("terminal too small for process selection (need 20 columns and 4 rows)")); }
            picker.resize(usize::from(cols), usize::from(rows))?;
            picker.render(&mut output, usize::from(cols))?;
            let event = reader.read_event();
            let result = match event {
                InputEvent::Signal(sig) if sig == libc::SIGTERM || sig == libc::SIGHUP => {
                    break Ok(PickOutcome::Terminated(sig));
                }
                InputEvent::Signal(sig) if sig == libc::SIGINT || sig == libc::SIGQUIT => Some(fz::Outcome::Cancelled),
                InputEvent::Signal(_) | InputEvent::PasteRejected => None,
                InputEvent::Paste(text) => { picker.paste(text.as_bytes()); None }
                InputEvent::Key(key) => {
                    let sequence: Vec<u8> = match (key.key, key.mods.ctrl) {
                        (Key::Escape, _) => { break Ok(PickOutcome::Selected(fz::Outcome::Cancelled)); }
                        (Key::Char('d'), true) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
                        (Key::Char(c), true) if c.is_ascii_alphabetic() => vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
                        (Key::Char(c), false) => { let mut bytes = [0; 4]; c.encode_utf8(&mut bytes).as_bytes().to_vec() }
                        (Key::Enter, _) => vec![b'\r'],
                        (Key::Up, _) => b"\x1b[A".to_vec(), (Key::Down, _) => b"\x1b[B".to_vec(),
                        (Key::Left, _) => b"\x1b[D".to_vec(), (Key::Right, _) => b"\x1b[C".to_vec(),
                        (Key::Home, _) => vec![1], (Key::End, _) => vec![5],
                        (Key::Backspace, _) => vec![127], (Key::Tab, _) => vec![9],
                        _ => Vec::new(),
                    };
                    let mut outcome = None;
                    for byte in sequence { outcome = picker.feed(Some(byte)); }
                    outcome
                }
            };
            if let Some(outcome) = result { break Ok(PickOutcome::Selected(outcome)); }
        }
    })();
    let mut output = io::stdout().lock();
    let cleanup = picker.clear(&mut output)
        .and_then(|()| output.write_all(b"\x1b[?2004l\x1b[0m\x1b[?25h"))
        .and_then(|()| output.flush())
        .and_then(|()| rustix::termios::tcsetattr(crate::term::stdin(), rustix::termios::OptionalActions::Now, &restore.0).map_err(io::Error::from));
    drop(restore);
    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    fn meta(pid: i32, parent: i32) -> Metadata {
        Metadata { pid: Pid::new(pid).unwrap(), parent, real_uid: 502, effective_uid: 502,
            started: (123, pid as u64), executable: b"fixture".to_vec(), argv: b"fixture\0".to_vec() }
    }
    struct Fake {
        records: RefCell<HashMap<Pid, Metadata>>,
        sends: RefCell<Vec<(Pid, rustix::process::Signal)>>,
        listener: Cell<bool>,
        bind_fails: Cell<bool>,
        inaccessible: Cell<bool>,
    }
    impl Fake {
        fn new() -> Self { Self { records: RefCell::new([meta(20, 10), meta(10, 1), meta(30, 20), meta(40, 20)].into_iter().map(|m| (m.pid, m)).collect()), sends: RefCell::new(vec![]), listener: Cell::new(true), bind_fails: Cell::new(false), inaccessible: Cell::new(false) } }
    }
    impl Backend for Fake {
        type Handle = Pid;
        fn list(&self) -> io::Result<Vec<Pid>> { Ok(self.records.borrow().keys().copied().collect()) }
        fn metadata(&self, pid: Pid) -> io::Result<Metadata> {
            if self.inaccessible.get() && pid.raw() >= 30 { return Err(io::Error::from_raw_os_error(libc::EACCES)); }
            self.records.borrow().get(&pid).cloned().ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
        }
        fn bind(&self, m: &Metadata) -> io::Result<Pid> { if self.bind_fails.get() { Err(io::Error::from_raw_os_error(libc::EMFILE)) } else { Ok(m.pid) } }
        fn listens(&self, _: Pid, _: u16) -> io::Result<bool> { Ok(self.listener.get()) }
        fn signal(&self, pid: &Pid, signal: rustix::process::Signal) -> io::Result<()> { self.sends.borrow_mut().push((*pid, signal)); Ok(()) }
    }
    #[test]
    fn cli_ports_and_operand_boundary() {
        for value in ["", "0", "65536", "-1", "+1", "1x", "999999999999999999999999"] {
            assert!(parse(&["-p".into(), value.into()]).is_err(), "{value}");
        }
        for args in [vec![], vec!["-p".into()], vec!["--".into()], vec!["--unknown".into()]] { assert!(parse(&args).is_err()); }
        assert_eq!(parse(&["-9".into(), "-p".into(), "00080".into(), "worker".into(), "--force".into()]).unwrap(), Command::Select { signal: rustix::process::Signal::KILL, port: Some(80), query: b"worker --force".to_vec() });
        assert_eq!(parse(&["--".into(), "-9".into()]).unwrap(), Command::Select { signal: rustix::process::Signal::TERM, port: None, query: b"-9".to_vec() });
    }
    #[test]
    fn protects_ancestors_but_retains_normal_shell_children() {
        let backend = Fake::new();
        let snapshot = snapshot(&backend, Pid::new(20).unwrap(), 502, None).unwrap();
        assert_eq!(snapshot.records.iter().map(|r| r.metadata.pid.raw()).collect::<Vec<_>>(), [30, 40]);
        assert_eq!(snapshot.protected, HashSet::from([Pid::new(20).unwrap(), Pid::new(10).unwrap()]));
    }
    #[test]
    fn invalid_batch_never_sends_even_its_valid_prefix() {
        let backend = Fake::new();
        let mut snapshot = snapshot(&backend, Pid::new(20).unwrap(), 502, None).unwrap();
        for ids in [&[0, 2][..], &[0, 0]] { assert!(send(&backend, &snapshot, ids, rustix::process::Signal::TERM, || None).is_err()); }
        snapshot.records[1].metadata = snapshot.records[0].metadata.clone();
        assert!(send(&backend, &snapshot, &[0, 1], rustix::process::Signal::TERM, || None).is_err());
        snapshot.records[0].metadata.pid = Pid::new(20).unwrap();
        assert!(send(&backend, &snapshot, &[0], rustix::process::Signal::TERM, || None).is_err());
        assert!(backend.sends.borrow().is_empty());
    }
    #[test]
    fn skips_changed_exited_and_no_longer_listening_records() {
        for change in 0..6 {
            let backend = Fake::new();
            let snapshot = snapshot(&backend, Pid::new(20).unwrap(), 502, Some(8080)).unwrap();
            let pid = snapshot.records[0].metadata.pid;
            if change == 0 { backend.records.borrow_mut().remove(&pid); }
            else if change == 5 { backend.listener.set(false); }
            else {
                let mut records = backend.records.borrow_mut();
                let record = records.get_mut(&pid).unwrap();
                match change { 1 => record.started.1 += 1, 2 => record.effective_uid = 0, 3 => record.parent = 1, _ => record.argv.push(b'x') }
            }
            let result = send(&backend, &snapshot, &[0, 1], rustix::process::Signal::TERM, || None).unwrap();
            assert!(result.failed.iter().any(|(failed, _)| *failed == pid));
            assert!(!backend.sends.borrow().iter().any(|(sent, _)| *sent == pid));
            if change != 5 { assert_eq!(result.sent, [snapshot.records[1].metadata.pid]); }
        }
    }
    #[test]
    fn exactly_selected_order_signal_and_interruption() {
        let backend = Fake::new();
        let snapshot = snapshot(&backend, Pid::new(20).unwrap(), 502, None).unwrap();
        let mut checks = 0;
        let batch = send(&backend, &snapshot, &[1, 0], rustix::process::Signal::KILL, || { checks += 1; (checks == 3).then_some(libc::SIGTERM) }).unwrap();
        assert_eq!(batch.sent, [snapshot.records[1].metadata.pid]);
        assert_eq!(batch.termination, Some(libc::SIGTERM));
        assert_eq!(*backend.sends.borrow(), [(snapshot.records[1].metadata.pid, rustix::process::Signal::KILL)]);
    }
    #[test]
    fn empty_eligible_set_is_distinct_from_failed_inspection() {
        let backend = Fake::new();
        for record in backend.records.borrow_mut().values_mut().filter(|record| record.pid.raw() >= 30) { record.effective_uid = 0; }
        assert!(snapshot(&backend, Pid::new(20).unwrap(), 502, None).unwrap().records.is_empty());
        backend.inaccessible.set(true);
        assert_eq!(snapshot(&backend, Pid::new(20).unwrap(), 502, None).err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert!(backend.sends.borrow().is_empty());
    }

    #[test]
    fn resource_failure_is_not_an_empty_snapshot_or_pid_fallback() {
        let backend = Fake::new(); backend.bind_fails.set(true);
        assert_eq!(snapshot(&backend, Pid::new(20).unwrap(), 502, None).err().unwrap().raw_os_error(), Some(libc::EMFILE));
        assert!(backend.sends.borrow().is_empty());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn native_bound_handle_rejects_an_exited_owned_fixture() {
        let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let backend = Native;
        let pid = Pid::new(child.id() as i32).unwrap();
        let metadata = backend.metadata(pid).unwrap();
        let handle = backend.bind(&metadata).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(backend.signal(&handle, rustix::process::Signal::TERM).is_err());
    }

    #[test]
    fn inherited_tcp_listeners_have_distinct_process_holders() {
        use std::os::unix::process::CommandExt;
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fd = listener.as_raw_fd();
        struct Children(Vec<std::process::Child>);
        impl Drop for Children { fn drop(&mut self) { for child in &mut self.0 { let _ = child.kill(); let _ = child.wait(); } } }
        let mut children = Children(Vec::new());
        for _ in 0..2 {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "read unused"]).stdin(std::process::Stdio::piped());
            // SAFETY: the fixture listener is live through spawn; only fcntl
            // runs before exec, preserving this one descriptor in each child.
            unsafe { command.pre_exec(move || { if libc::fcntl(fd, libc::F_SETFD, 0) == -1 { Err(io::Error::last_os_error()) } else { Ok(()) } }); }
            children.0.push(command.spawn().unwrap());
        }
        let backend = Native;
        for child in &children.0 { assert!(backend.listens(Pid::new(child.id() as i32).unwrap(), port).unwrap()); }
        drop(listener);
        for child in &children.0 { assert!(backend.listens(Pid::new(child.id() as i32).unwrap(), port).unwrap()); }
    }

    #[test]
    fn reuseport_listeners_remain_eligible_until_the_last_socket_closes() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        fn listener(port: u16) -> std::net::TcpListener {
            #[cfg(target_os = "linux")]
            let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
            #[cfg(target_os = "macos")]
            let flags = libc::SOCK_STREAM;
            // SAFETY: scalar socket arguments request a private owned TCP socket.
            let raw = unsafe { libc::socket(libc::AF_INET, flags, 0) };
            assert!(raw >= 0);
            // SAFETY: socket returned this newly owned descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).unwrap();
            let one = 1i32;
            // SAFETY: one is a correctly sized integer socket option.
            assert_eq!(unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT, (&one as *const i32).cast(), std::mem::size_of_val(&one) as libc::socklen_t) }, 0);
            // SAFETY: zeroed sockaddr_in admits all bit patterns and is filled before use.
            let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            #[cfg(target_os = "macos")]
            { address.sin_len = std::mem::size_of_val(&address) as u8; }
            address.sin_port = port.to_be();
            address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            // SAFETY: address is initialized and sized as the IPv4 socket ABI expects.
            assert_eq!(unsafe { libc::bind(fd.as_raw_fd(), (&address as *const libc::sockaddr_in).cast(), std::mem::size_of_val(&address) as libc::socklen_t) }, 0);
            // SAFETY: fd is the bound TCP socket owned by this fixture.
            assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 8) }, 0);
            fd.into()
        }
        let first = listener(0);
        let port = first.local_addr().unwrap().port();
        let second = listener(port);
        let pid = Pid::new(rustix::process::getpid().as_raw_pid()).unwrap();
        assert!(Native.listens(pid, port).unwrap());
        drop(first);
        assert!(Native.listens(pid, port).unwrap());
        drop(second);
        assert!(!Native.listens(pid, port).unwrap());
    }

    #[test]
    fn native_tcp_ownership_ipv4_ipv6_wildcard_and_socket_closure() {
        let backend = Native;
        let pid = Pid::new(rustix::process::getpid().as_raw_pid()).unwrap();
        for address in ["127.0.0.1:0", "0.0.0.0:0", "[::1]:0", "[::]:0"] {
            let listener = match std::net::TcpListener::bind(address) {
                Ok(listener) => listener,
                Err(error) if address.starts_with('[') && error.kind() == io::ErrorKind::AddrNotAvailable => continue,
                Err(error) => panic!("{address}: {error}"),
            };
            let port = listener.local_addr().unwrap().port();
            assert!(backend.listens(pid, port).unwrap(), "{address}");
            let shared = listener.try_clone().unwrap();
            drop(listener);
            assert!(backend.listens(pid, port).unwrap());
            drop(shared);
            assert!(!backend.listens(pid, port).unwrap());
            let udp = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
            assert!(!backend.listens(pid, udp.local_addr().unwrap().port()).unwrap());
        }
    }

    #[test]
    fn display_cannot_emit_controls_or_lose_invalid_bytes() {
        let display = display_bytes(b"a\0b\n\x1b[31m\xff");
        assert_eq!(display, b"a b\\n\\u{1b}[31m\\xff");
    }
}
