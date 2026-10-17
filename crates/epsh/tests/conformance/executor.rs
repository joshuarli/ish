use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::process::{kill_process_group, Pid, Signal};

use super::fixture::CaseScratch;
use super::parser::{Case, ExpectedExit};
use super::pattern::OutputAssertion;

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub failure: Option<String>,
}

#[derive(Debug)]
enum Stream {
    Stdout,
    Stderr,
}

enum ReaderMessage {
    Complete(Stream, Vec<u8>),
    TooLarge(Stream, Vec<u8>),
    ReadError(Stream, Vec<u8>, io::Error),
}

pub fn execute_case(case: &Case, binary: &OsStr) -> Result<ProcessOutput, String> {
    let scratch = CaseScratch::create(case)?;
    let run = build_command(case, binary, &scratch)
        .and_then(|mut command| run_process(&mut command, Duration::from_secs(case.time_limit_secs)));
    let cleanup = scratch.close();
    match (run, cleanup) {
        (Ok(output), Ok(())) => Ok(output),
        (Ok(_), Err(cleanup)) => Err(format!("{}: {cleanup}", case.name)),
        (Err(run), Ok(())) => Err(format!("{}: {run}", case.name)),
        (Err(run), Err(cleanup)) => Err(format!("{}: {run}; {cleanup}", case.name)),
    }
}

fn build_command(case: &Case, binary: &OsStr, scratch: &CaseScratch) -> Result<Command, String> {
    let mut command = Command::new(binary);
    for argument in &case.arguments {
        command.arg(OsString::from_vec(argument.clone()));
    }
    if let Some(script) = &scratch.script {
        command.arg(script);
    }
    let stdin = if case.stdin.is_some() {
        File::open(&scratch.stdin)
    } else {
        File::open("/dev/null")
    }
    .map_err(|error| format!("cannot open test stdin: {error}"))?;

    command.current_dir(&scratch.cwd);
    command.env_clear();
    for name in [
        "HOME",
        "LD_LIBRARY_PATH",
        "LOCPATH",
        "LOGNAME",
        "PATH",
        "PERLIO",
        "SHELL",
        "UNIXMODE",
        "UNIXROOT",
        "USER",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("CYGWIN", "nodosfilewarning");
    command.env("ENV", "/nonexisting");
    command.env("LANG", "C");
    for (key, value) in &case.environment {
        let key = OsString::from_vec(key.clone());
        if let Some(value) = value {
            command.env(key, OsString::from_vec(value.clone()));
        } else {
            command.env_remove(key);
        }
    }
    command.env("__progname", binary);
    command.stdin(Stdio::from(stdin));
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);
    Ok(command)
}

fn run_process(command: &mut Command, timeout: Duration) -> Result<ProcessOutput, String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "test time limit is too large".to_owned())?;
    let mut child = command.spawn().map_err(|error| format!("cannot start test program: {error}"))?;
    let process_group = match i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
        Some(process_group) => process_group,
        None => {
            let kill = child.kill().map_err(|error| format!("cannot kill test program: {error}"));
            let wait = child.wait().map(|_| ()).map_err(|error| format!("cannot reap test program: {error}"));
            return Err(combine_errors("test process ID is outside the supported range", kill, wait));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return stop_after_error(&mut child, process_group, None, None, "test stdout pipe is missing".to_owned()),
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return stop_after_error(&mut child, process_group, None, None, "test stderr pipe is missing".to_owned()),
    };
    let (sender, receiver) = mpsc::sync_channel(2);
    let stdout_reader = match start_reader(stdout, Stream::Stdout, sender.clone()) {
        Ok(reader) => reader,
        Err(error) => return stop_after_error(&mut child, process_group, None, None, error),
    };
    let stderr_reader = match start_reader(stderr, Stream::Stderr, sender) {
        Ok(reader) => reader,
        Err(error) => return stop_after_error(&mut child, process_group, Some(stdout_reader), None, error),
    };
    wait_for_process(&mut child, process_group, deadline, &receiver, stdout_reader, stderr_reader)
}

fn start_reader<R: Read + Send + 'static>(
    reader: R,
    stream: Stream,
    sender: SyncSender<ReaderMessage>,
) -> Result<JoinHandle<()>, String> {
    let name = match stream {
        Stream::Stdout => "epsh-conformance-stdout",
        Stream::Stderr => "epsh-conformance-stderr",
    };
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || drain_output(reader, stream, sender))
        .map_err(|error| format!("cannot start output reader: {error}"))
}

fn drain_output<R: Read>(mut reader: R, stream: Stream, sender: SyncSender<ReaderMessage>) {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(ReaderMessage::Complete(stream, output));
                return;
            }
            Ok(size) if output.len().saturating_add(size) > OUTPUT_LIMIT => {
                let available = OUTPUT_LIMIT.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..available]);
                let _ = sender.send(ReaderMessage::TooLarge(stream, output));
                return;
            }
            Ok(size) => output.extend_from_slice(&buffer[..size]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = sender.send(ReaderMessage::ReadError(stream, output, error));
                return;
            }
        }
    }
}

fn wait_for_process(
    child: &mut Child,
    process_group: Pid,
    deadline: Instant,
    receiver: &Receiver<ReaderMessage>,
    stdout_reader: JoinHandle<()>,
    stderr_reader: JoinHandle<()>,
) -> Result<ProcessOutput, String> {
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let mut failure = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => failure = Some(format!("cannot wait for test program: {error}")),
            }
        }
        while let Ok(message) = receiver.try_recv() {
            record_message(message, &mut stdout, &mut stderr, &mut failure);
        }
        if failure.is_some() {
            break;
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            failure = Some("test exceeded its time limit".to_owned());
            break;
        }
        let wait = (deadline - now).min(POLL_INTERVAL);
        match receiver.recv_timeout(wait) {
            Ok(message) => record_message(message, &mut stdout, &mut stderr, &mut failure),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) if stdout.is_some() && stderr.is_some() => {
                thread::sleep(wait);
            }
            Err(RecvTimeoutError::Disconnected) => {
                failure = Some("output reader stopped without a result".to_owned());
            }
        }
    }

    if let Some(reason) = failure {
        let cleanup = stop_child(child, process_group);
        let joined = join_readers(stdout_reader, stderr_reader);
        let mut details = vec![reason];
        if let Err(error) = cleanup {
            details.push(error);
        }
        if let Err(error) = joined {
            details.push(error);
        }
        let mut reader_failure = None;
        while let Ok(message) = receiver.try_recv() {
            record_message(message, &mut stdout, &mut stderr, &mut reader_failure);
        }
        if let Some(error) = reader_failure {
            details.push(error);
        }
        return Ok(ProcessOutput {
            status: child.try_wait().ok().flatten(),
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
            failure: Some(details.join("; ")),
        });
    }

    if let Err(error) = join_readers(stdout_reader, stderr_reader) {
        return Err(error);
    }
    Ok(ProcessOutput {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
        failure: None,
    })
}

fn record_message(
    message: ReaderMessage,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    failure: &mut Option<String>,
) {
    match message {
        ReaderMessage::Complete(stream, output) => set_stream(stream, output, stdout, stderr, failure),
        ReaderMessage::TooLarge(stream, output) => {
            let name = stream_name(&stream);
            set_stream(stream, output, stdout, stderr, failure);
            *failure = Some(format!("{name} exceeded the 8 MiB output limit"));
        }
        ReaderMessage::ReadError(stream, output, error) => {
            let name = stream_name(&stream);
            set_stream(stream, output, stdout, stderr, failure);
            *failure = Some(format!("cannot read {name}: {error}"));
        }
    }
}

fn set_stream(
    stream: Stream,
    output: Vec<u8>,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    failure: &mut Option<String>,
) {
    let slot = match stream {
        Stream::Stdout => stdout,
        Stream::Stderr => stderr,
    };
    if slot.replace(output).is_some() {
        *failure = Some("output reader returned twice".to_owned());
    }
}

fn stream_name(stream: &Stream) -> &'static str {
    match stream {
        Stream::Stdout => "stdout",
        Stream::Stderr => "stderr",
    }
}

fn stop_after_error(
    child: &mut Child,
    process_group: Pid,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    primary: String,
) -> Result<ProcessOutput, String> {
    let cleanup = stop_child(child, process_group);
    let readers = match (stdout_reader, stderr_reader) {
        (Some(stdout), Some(stderr)) => join_readers(stdout, stderr),
        (Some(stdout), None) => stdout.join().map_err(|_| "stdout reader panicked".to_owned()),
        (None, Some(stderr)) => stderr.join().map_err(|_| "stderr reader panicked".to_owned()),
        (None, None) => Ok(()),
    };
    Err(combine_errors(&primary, cleanup, readers))
}

fn combine_errors(
    primary: &str,
    cleanup: Result<(), String>,
    readers: Result<(), String>,
) -> String {
    let mut errors = vec![primary.to_owned()];
    if let Err(error) = cleanup {
        errors.push(error);
    }
    if let Err(error) = readers {
        errors.push(error);
    }
    errors.join("; ")
}

fn stop_child(child: &mut Child, process_group: Pid) -> Result<(), String> {
    let signal = match kill_process_group(process_group, Signal::KILL) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(()),
        Err(error) => Err(format!("cannot kill test process group: {error}")),
    };
    let wait = child.wait()
        .map(|_| ())
        .map_err(|error| format!("cannot reap test process: {error}"));
    match (signal, wait) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(signal), Ok(())) => Err(signal),
        (Ok(()), Err(wait)) => Err(wait),
        (Err(signal), Err(wait)) => Err(format!("{signal}; {wait}")),
    }
}

fn join_readers(stdout: JoinHandle<()>, stderr: JoinHandle<()>) -> Result<(), String> {
    let stdout = stdout.join().map_err(|_| "stdout reader panicked".to_owned());
    let stderr = stderr.join().map_err(|_| "stderr reader panicked".to_owned());
    match (stdout, stderr) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(stdout), Ok(())) => Err(stdout),
        (Ok(()), Err(stderr)) => Err(stderr),
        (Err(stdout), Err(stderr)) => Err(format!("{stdout}; {stderr}")),
    }
}

pub fn assertion_failures(case: &Case, output: &ProcessOutput) -> Vec<String> {
    let mut failures = Vec::new();
    if let Some(reason) = &output.failure {
        failures.push(reason.clone());
    }
    let exit_matches = match (case.expected_exit, output.status.as_ref()) {
        (ExpectedExit::Code(expected), Some(status)) => status.code() == Some(i32::from(expected)),
        (ExpectedExit::IsZero, Some(status)) => status.code() == Some(0),
        (ExpectedExit::IsNonzero, Some(status)) => status.code() != Some(0),
        (_, None) => false,
    };
    if !exit_matches {
        failures.push(format!(
            "exit status mismatch: expected {:?}, got {}",
            case.expected_exit,
            output.status.as_ref().map_or_else(|| "no status".to_owned(), display_status)
        ));
    }
    compare_output("stdout", case.stdout.as_ref(), &output.stdout, &mut failures);
    compare_output("stderr", case.stderr.as_ref(), &output.stderr, &mut failures);
    failures
}

fn compare_output(
    stream: &str,
    assertion: Option<&OutputAssertion>,
    actual: &[u8],
    failures: &mut Vec<String>,
) {
    let empty = OutputAssertion::Exact(Vec::new());
    let assertion = assertion.unwrap_or(&empty);
    match assertion.matches(actual) {
        Ok(true) => {}
        Ok(false) => failures.push(format!("{stream} mismatch: wanted {assertion:?}, got {actual:?}")),
        Err(error) => failures.push(format!("{stream} pattern failed: {error}")),
    }
}

fn display_status(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => format!("exit {code}"),
        None => format!("signal {}", status.signal().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use super::run_process;

    #[test]
    fn timeout_kills_the_process_group_and_reaps_the_child() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & echo descendant=$!; wait"]);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let output = run_process(&mut command, Duration::from_millis(200)).unwrap();
        assert!(output.failure.as_deref().unwrap().contains("time limit"));
        assert!(output.status.is_some());
        let descendant = output.stdout.strip_prefix(b"descendant=").unwrap();
        let descendant = std::str::from_utf8(descendant.strip_suffix(b"\n").unwrap()).unwrap();
        assert!(descendant.parse::<u32>().is_ok());
    }

    #[test]
    fn waits_for_exit_after_both_output_streams_close() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec 1>&- 2>&-; sleep 0.1"]);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let output = run_process(&mut command, Duration::from_secs(1)).unwrap();
        assert!(output.failure.is_none());
        assert_eq!(output.status.unwrap().code(), Some(0));
    }
}
