use crate::{signal, term};
use ish_fs::{Cancellation, Event, EventSink, Progress};
use std::io::{self, IsTerminal, Write};
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const PROGRESS_DELAY: Duration = Duration::from_millis(200);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const BLOCKED_REPORT_AFTER: Duration = Duration::from_secs(30);
const EVENT_CAPACITY: usize = 256;
const MAX_CONFIRMATION_BYTES: usize = 256;

#[derive(Default)]
struct ProgressReplay {
    latest: Option<Progress>,
    last_delivery: Option<Instant>,
}

impl ProgressReplay {
    fn delivered(&mut self, progress: Progress, now: Instant) -> Progress {
        let latest = self.latest.get_or_insert(progress);
        merge_progress(latest, progress);
        self.last_delivery = Some(now);
        *latest
    }

    fn due(&self, started: Instant, now: Instant) -> Option<Progress> {
        if now.saturating_duration_since(started) < PROGRESS_DELAY
            || self.last_delivery.is_some_and(|last| {
                now.saturating_duration_since(last) < PROGRESS_INTERVAL
            })
        {
            return None;
        }
        self.latest
    }
}

struct ActivityWatchdog {
    last_activity: Instant,
    latest_progress: Progress,
    reported: bool,
}

impl ActivityWatchdog {
    fn new(now: Instant) -> Self {
        Self {
            last_activity: now,
            latest_progress: Progress::default(),
            reported: false,
        }
    }

    fn observe(&mut self, event: &Event, now: Instant) {
        match event {
            Event::Progress(progress) => {
                let mut latest = self.latest_progress;
                merge_progress(&mut latest, *progress);
                if latest != self.latest_progress {
                    self.latest_progress = latest;
                    self.note_activity(now);
                }
            }
            Event::Operand(_) | Event::Diagnostic(_) => self.note_activity(now),
        }
    }

    fn note_activity(&mut self, now: Instant) {
        self.last_activity = now;
        self.reported = false;
    }

    fn report_due(&mut self, now: Instant) -> bool {
        if !self.reported
            && now.saturating_duration_since(self.last_activity) >= BLOCKED_REPORT_AFTER
        {
            self.reported = true;
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
enum QueuedEvent {
    Event(Event),
    ProgressReady,
}

struct EventQueue {
    sender: SyncSender<QueuedEvent>,
    latest_progress: Arc<Mutex<Option<Progress>>>,
    progress_pending: Arc<AtomicBool>,
}

impl EventSink for EventQueue {
    fn emit(&self, event: Event) -> io::Result<()> {
        match event {
            Event::Progress(progress) => {
                let mut latest = self
                    .latest_progress
                    .lock()
                    .map_err(|_| io::Error::other("foreground progress state was poisoned"))?;
                if let Some(current) = latest.as_mut() {
                    merge_progress(current, progress);
                } else {
                    *latest = Some(progress);
                }
                drop(latest);
                if !self.progress_pending.swap(true, Ordering::AcqRel) {
                    match self.sender.try_send(QueuedEvent::ProgressReady) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => {
                            self.progress_pending.store(false, Ordering::Release);
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "foreground event receiver stopped",
                            ));
                        }
                    }
                }
                Ok(())
            }
            event => self
                .sender
                .send(QueuedEvent::Event(event))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "foreground event receiver stopped")),
        }
    }
}

pub struct Foreground {
    cancellation: Cancellation,
    _signals: signal::ForegroundSignals,
    _terminal: term::ForegroundMode,
    started: Instant,
    watchdog: ActivityWatchdog,
    last_progress: Option<Instant>,
    progress_replay: ProgressReplay,
    progress_drawn: bool,
    output_error: Option<io::Error>,
}

impl Foreground {
    pub fn start() -> io::Result<Self> {
        let signals = signal::ForegroundSignals::install()?;
        let terminal = term::ForegroundMode::enable()?;
        let now = Instant::now();
        Ok(Self {
            cancellation: Cancellation::default(),
            _signals: signals,
            _terminal: terminal,
            started: now,
            watchdog: ActivityWatchdog::new(now),
            last_progress: None,
            progress_replay: ProgressReplay::default(),
            progress_drawn: false,
            output_error: None,
        })
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub fn poll(&mut self) {
        while let Some(sig) = signal::read_signal() {
            self.handle_signal(sig);
        }
    }

    pub fn recv_timeout<T>(
        &mut self,
        receiver: &Receiver<T>,
    ) -> Result<T, RecvTimeoutError> {
        self.poll();
        self.check_watchdog();
        let received = receiver.recv_timeout(POLL_INTERVAL);
        self.poll();
        self.check_watchdog();
        match received {
            Ok(event) => {
                self.watchdog.note_activity(Instant::now());
                Ok(event)
            }
            Err(RecvTimeoutError::Timeout) => Err(RecvTimeoutError::Timeout),
            Err(error) => Err(error),
        }
    }

    pub fn join<T>(&mut self, worker: JoinHandle<T>) -> thread::Result<T> {
        while !worker.is_finished() {
            self.poll();
            self.check_watchdog();
            thread::park_timeout(POLL_INTERVAL);
        }
        self.poll();
        worker.join()
    }

    pub fn report_progress(
        &mut self,
        label: &str,
        completed: u64,
        total: Option<u64>,
    ) -> io::Result<()> {
        let line = match total {
            Some(total) => format!("{label} {completed}/{total}"),
            None => format!("{label} {completed}"),
        };
        self.report_progress_line(&line)
    }

    pub fn report_progress_line(&mut self, line: &str) -> io::Result<()> {
        if !io::stderr().is_terminal()
            || self.started.elapsed() < PROGRESS_DELAY
            || self
                .last_progress
                .is_some_and(|last| last.elapsed() < PROGRESS_INTERVAL)
        {
            return Ok(());
        }
        let result = write!(io::stderr(), "\r\x1b[2K{line}")
            .and_then(|()| io::stderr().flush());
        match result {
            Ok(()) => {
                self.last_progress = Some(Instant::now());
                self.progress_drawn = true;
                Ok(())
            }
            Err(error) => {
                self.note_output_error(error);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "foreground progress output failed"))
            }
        }
    }

    pub fn clear_progress(&mut self) -> io::Result<()> {
        if !self.progress_drawn {
            return Ok(());
        }
        self.progress_drawn = false;
        let result = write!(io::stderr(), "\r\x1b[2K").and_then(|()| io::stderr().flush());
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.note_output_error(error);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "foreground progress output failed"))
            }
        }
    }

    pub fn report_diagnostic(
        &mut self,
        path: &std::path::Path,
        message: &str,
    ) -> io::Result<()> {
        self.clear_progress()?;
        let path = escape_path(path);
        let message = escape_terminal_text(message);
        match writeln!(io::stderr(), "ish: {path}: {message}")
            .and_then(|()| io::stderr().flush())
        {
            Ok(()) => Ok(()),
            Err(error) => {
                self.note_output_error(error);
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "foreground diagnostic output failed",
                ))
            }
        }
    }

    pub fn read_confirmation_line(&mut self) -> io::Result<Option<String>> {
        let mut nonblocking = NonblockingStdin::enable(0)?;
        let result = self.read_confirmation_nonblocking(0);
        let restore = nonblocking.restore();
        match (result, restore) {
            (Ok(answer), Ok(())) => Ok(answer),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(restore_error)) => Err(io::Error::other(format!(
                "{error}; failed to restore stdin flags: {restore_error}"
            ))),
        }
    }

    fn read_confirmation_nonblocking(&mut self, stdin_fd: RawFd) -> io::Result<Option<String>> {
        let stdin = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let mut answer = Vec::with_capacity(4);
        let mut too_long = false;
        let finish_line = |answer: Vec<u8>, too_long: bool| -> io::Result<Option<String>> {
            if too_long {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "confirmation answer exceeds 256 bytes",
                ));
            }
            if answer.is_empty() {
                return Ok(None);
            }
            String::from_utf8(answer)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        };
        loop {
            self.poll();
            if self.cancellation.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled"));
            }
            match poll_stdin(stdin_fd)? {
                Some(true) => {
                    self.watchdog.note_activity(Instant::now());
                    let mut byte = [0u8; 1];
                    match rustix::io::read(stdin, &mut byte) {
                        Ok(0) => return finish_line(answer, too_long),
                        Ok(_) if byte[0] == b'\n' => {
                            if !too_long {
                                answer.push(b'\n');
                            }
                            return finish_line(answer, too_long);
                        }
                        Ok(_) if answer.len() < MAX_CONFIRMATION_BYTES => answer.push(byte[0]),
                        Ok(_) => too_long = true,
                        Err(error)
                            if error == rustix::io::Errno::AGAIN
                                || error == rustix::io::Errno::INTR => {}
                        Err(error) => return Err(io::Error::from(error)),
                    }
                }
                Some(false) => self.poll(),
                None => return finish_line(answer, too_long),
            }
        }
    }

    pub fn run<T, F, H>(&mut self, operation: F, mut handle_event: H) -> io::Result<T>
    where
        T: Send,
        F: FnOnce(Cancellation, &dyn EventSink) -> T + Send,
        H: FnMut(&mut Foreground, Event) -> io::Result<()>,
    {
        let (sender, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
        let latest_progress = Arc::new(Mutex::new(None));
        let progress_pending = Arc::new(AtomicBool::new(false));
        let event_queue = EventQueue {
            sender,
            latest_progress: Arc::clone(&latest_progress),
            progress_pending: Arc::clone(&progress_pending),
        };
        let cancellation = self.cancellation.clone();
        let mut output_error = None;
        let result = thread::scope(|scope| -> io::Result<T> {
            let worker = thread::Builder::new()
                .spawn_scoped(scope, move || operation(cancellation, &event_queue))?;
            loop {
                self.poll();
                if worker.is_finished() {
                    loop {
                        self.poll();
                        match receiver.try_recv() {
                            Ok(queued) => self.handle_queued_event(
                                queued,
                                &latest_progress,
                                &progress_pending,
                                &mut handle_event,
                                &mut output_error,
                            ),
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }
                    self.deliver_latest_progress(
                        &latest_progress,
                        &progress_pending,
                        &mut handle_event,
                        &mut output_error,
                    );
                    break;
                }
                self.check_watchdog();
                match receiver.recv_timeout(POLL_INTERVAL) {
                    Ok(queued) => {
                        if !worker.is_finished() {
                            self.check_watchdog();
                        }
                        self.handle_queued_event(
                            queued,
                            &latest_progress,
                            &progress_pending,
                            &mut handle_event,
                            &mut output_error,
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !worker.is_finished() {
                            self.check_watchdog();
                        }
                        self.poll();
                        self.deliver_latest_progress(
                            &latest_progress,
                            &progress_pending,
                            &mut handle_event,
                            &mut output_error,
                        );
                        self.replay_progress(&mut handle_event, &mut output_error);
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        if !worker.is_finished() {
                            self.check_watchdog();
                        }
                        self.replay_progress(&mut handle_event, &mut output_error);
                        thread::park_timeout(POLL_INTERVAL);
                    }
                }
            }
            worker
                .join()
                .map_err(|_| io::Error::other("foreground operation worker panicked"))
        });
        if let Err(error) = self.clear_progress() {
            let stored_error = self.output_error.take();
            output_error.get_or_insert(stored_error.unwrap_or(error));
        }
        let value = result?;
        let stored_error = self.output_error.take();
        if let Some(error) = output_error.or(stored_error) {
            Err(error)
        } else {
            Ok(value)
        }
    }

    pub fn finish(&mut self) -> io::Result<()> {
        let clear = self.clear_progress();
        match (clear, self.output_error.take()) {
            (Err(error), _) => Err(error),
            (Ok(()), Some(error)) => Err(error),
            (Ok(()), None) => Ok(()),
        }
    }

    fn handle_signal(&mut self, sig: i32) {
        if sig == rustix::process::Signal::INT.as_raw() {
            self.cancellation.cancel();
        } else if sig == rustix::process::Signal::TERM.as_raw()
            || sig == rustix::process::Signal::HUP.as_raw()
        {
            self.cancellation.cancel();
            signal::note_termination(sig);
        }
    }

    fn handle_queued_event<H>(
        &mut self,
        queued: QueuedEvent,
        latest_progress: &Mutex<Option<Progress>>,
        progress_pending: &AtomicBool,
        handle_event: &mut H,
        output_error: &mut Option<io::Error>,
    ) where
        H: FnMut(&mut Foreground, Event) -> io::Result<()>,
    {
        match queued {
            QueuedEvent::Event(event) => self.deliver_event(event, handle_event, output_error),
            QueuedEvent::ProgressReady => self.deliver_latest_progress(
                latest_progress,
                progress_pending,
                handle_event,
                output_error,
            ),
        }
        self.deliver_latest_progress(
            latest_progress,
            progress_pending,
            handle_event,
            output_error,
        );
    }

    fn deliver_latest_progress<H>(
        &mut self,
        latest_progress: &Mutex<Option<Progress>>,
        progress_pending: &AtomicBool,
        handle_event: &mut H,
        output_error: &mut Option<io::Error>,
    ) where
        H: FnMut(&mut Foreground, Event) -> io::Result<()>,
    {
        let progress = match latest_progress.lock() {
            Ok(mut latest) => latest.take(),
            Err(_) => {
                self.note_output_error(io::Error::other("foreground progress state was poisoned"));
                return;
            }
        };
        progress_pending.store(false, Ordering::Release);
        if let Some(progress) = progress {
            self.deliver_event(Event::Progress(progress), handle_event, output_error);
        }
    }

    fn deliver_event<H>(
        &mut self,
        mut event: Event,
        handle_event: &mut H,
        output_error: &mut Option<io::Error>,
    ) where
        H: FnMut(&mut Foreground, Event) -> io::Result<()>,
    {
        let now = Instant::now();
        if let Event::Progress(progress) = &mut event {
            *progress = self.progress_replay.delivered(*progress, now);
        }
        self.watchdog.observe(&event, now);
        if let Err(error) = handle_event(self, event) {
            self.cancellation.cancel_for_output();
            let stored_error = self.output_error.take();
            output_error.get_or_insert(stored_error.unwrap_or(error));
        }
    }

    fn replay_progress<H>(
        &mut self,
        handle_event: &mut H,
        output_error: &mut Option<io::Error>,
    ) where
        H: FnMut(&mut Foreground, Event) -> io::Result<()>,
    {
        if output_error.is_some() {
            return;
        }
        if let Some(progress) = self.progress_replay.due(self.started, Instant::now()) {
            self.deliver_event(Event::Progress(progress), handle_event, output_error);
        }
    }

    fn note_output_error(&mut self, error: io::Error) {
        self.cancellation.cancel_for_output();
        self.output_error.get_or_insert(error);
    }

    fn check_watchdog(&mut self) {
        if self.watchdog.report_due(Instant::now())
            && let Err(error) = writeln!(
                io::stderr(),
                "ish: foreground operation has made no progress for 30 seconds; waiting for safe completion"
            )
            .and_then(|()| io::stderr().flush())
        {
            self.note_output_error(error);
        }
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.clear_progress();
    }
}

struct NonblockingStdin {
    fd: RawFd,
    original_flags: rustix::fs::OFlags,
    restored: bool,
}

impl NonblockingStdin {
    fn enable(fd: RawFd) -> io::Result<Self> {
        let stdin = unsafe { BorrowedFd::borrow_raw(fd) };
        let original_flags = rustix::fs::fcntl_getfl(stdin)?;
        rustix::fs::fcntl_setfl(stdin, original_flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self {
            fd,
            original_flags,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let stdin = unsafe { BorrowedFd::borrow_raw(self.fd) };
        rustix::fs::fcntl_setfl(stdin, self.original_flags)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for NonblockingStdin {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn merge_progress(current: &mut Progress, newer: Progress) {
    current.discovered_files = current.discovered_files.max(newer.discovered_files);
    current.completed_files = current.completed_files.max(newer.completed_files);
    current.skipped_files = current.skipped_files.max(newer.skipped_files);
    current.failed_files = current.failed_files.max(newer.failed_files);
    current.logical_bytes_cloned = current.logical_bytes_cloned.max(newer.logical_bytes_cloned);
    current.bytes_streamed = current.bytes_streamed.max(newer.bytes_streamed);
    current.cloned_files = current.cloned_files.max(newer.cloned_files);
    current.native_copied_files = current.native_copied_files.max(newer.native_copied_files);
    current.buffered_copied_files = current.buffered_copied_files.max(newer.buffered_copied_files);
}

fn poll_stdin(stdin_fd: RawFd) -> io::Result<Option<bool>> {
    let stdin = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
    let mut fds = [rustix::event::PollFd::from_borrowed_fd(
        stdin,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: POLL_INTERVAL.as_secs() as i64,
        tv_nsec: POLL_INTERVAL.subsec_nanos() as i64,
    };
    match rustix::event::poll(&mut fds, Some(&timeout)) {
        Ok(0) => Ok(Some(false)),
        Ok(_) if fds[0].revents().contains(rustix::event::PollFlags::IN) => Ok(Some(true)),
        Ok(_) if fds[0].revents().contains(rustix::event::PollFlags::HUP) => Ok(None),
        Ok(_) => Ok(Some(false)),
        Err(error) if error == rustix::io::Errno::INTR => Ok(Some(false)),
        Err(error) => Err(io::Error::from(error)),
    }
}

fn escape_path(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut escaped = String::new();
    for &byte in path.as_os_str().as_bytes() {
        append_escaped_byte(&mut escaped, byte);
    }
    escaped
}

fn escape_terminal_text(text: &str) -> String {
    let mut escaped = String::new();
    for byte in text.bytes() {
        append_escaped_byte(&mut escaped, byte);
    }
    escaped
}

fn append_escaped_byte(output: &mut String, byte: u8) {
    match byte {
        b' '..=b'~' if byte != b'\\' => output.push(byte as char),
        b'\\' => output.push_str("\\\\"),
        _ => {
            use std::fmt::Write as _;
            let _ = write!(output, "\\x{byte:02x}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ish_fs::{OperandOutcome, OperandState};
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::process::Command;

    #[cfg(target_os = "linux")]
    #[test]
    fn run_returns_spawn_error_when_thread_creation_is_denied() {
        const CHILD_MODE: &str = "ISH_THREAD_LIMIT_TEST_CHILD";
        let child_mode = std::env::var_os(CHILD_MODE).is_some();
        if child_mode {
            let mut foreground = Foreground::start().expect("foreground controller starts");
            rustix::process::setrlimit(
                rustix::process::Resource::Nproc,
                rustix::process::Rlimit { current: Some(0), maximum: Some(0) },
            ).expect("set RLIMIT_NPROC in the isolated test process");

            let called = Arc::new(AtomicBool::new(false));
            let called_by_operation = Arc::clone(&called);
            let result = foreground.run(
                move |_, _| {
                    called_by_operation.store(true, Ordering::SeqCst);
                    1
                },
                |_, _| Ok(()),
            );
            assert!(result.is_err());
            assert!(!called.load(Ordering::SeqCst));
            foreground.finish().expect("foreground guards finish after spawn failure");
            return;
        }

        if rustix::process::geteuid() == rustix::process::Uid::ROOT {
            eprintln!("skipped: RLIMIT_NPROC does not constrain root thread creation here");
            return;
        }
        let output = Command::new(std::env::current_exe().expect("test executable path is available"))
            .args([
                "--exact",
                "foreground::tests::run_returns_spawn_error_when_thread_creation_is_denied",
                "--nocapture",
            ])
            .env(CHILD_MODE, "1")
            .output()
            .expect("resource-limited test subprocess starts");
        assert!(
            output.status.success(),
            "resource-limited child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn watchdog_ignores_unchanged_progress_and_restarts_after_real_progress() {
        let started = Instant::now();
        let mut watchdog = ActivityWatchdog::new(started);
        let initial_progress = Progress {
            discovered_files: 10,
            completed_files: 8,
            ..Progress::default()
        };
        let initial = Event::Progress(initial_progress);
        watchdog.observe(&initial, started);
        let heartbeat = Event::Progress(Progress::default());
        for tick in 1..=1195 {
            let now = started + Duration::from_millis(tick * 25);
            watchdog.observe(&heartbeat, now);
            assert!(!watchdog.report_due(now));
        }
        let regressed = Event::Progress(Progress {
            discovered_files: 9,
            completed_files: 7,
            ..Progress::default()
        });
        let before_report = started + Duration::from_millis(29_900);
        watchdog.observe(&regressed, before_report);
        assert!(!watchdog.report_due(before_report));

        let threshold = started + Duration::from_secs(30);
        watchdog.observe(&heartbeat, threshold);
        assert!(watchdog.report_due(threshold));
        let after_report = started + Duration::from_millis(30_025);
        watchdog.observe(&heartbeat, after_report);
        assert!(!watchdog.report_due(after_report));

        let advanced = Event::Progress(Progress {
            discovered_files: 11,
            completed_files: 9,
            ..initial_progress
        });
        let resumed = started + Duration::from_secs(31);
        watchdog.observe(&advanced, resumed);
        assert!(!watchdog.report_due(resumed + Duration::from_millis(29_999)));
        assert!(watchdog.report_due(resumed + Duration::from_secs(30)));
    }

    #[test]
    fn progress_replay_obeys_delay_interval_and_latest_snapshot() {
        let started = Instant::now();
        let mut replay = ProgressReplay::default();
        let first = Progress {
            discovered_files: 1,
            completed_files: 1,
            ..Progress::default()
        };
        replay.delivered(first, started);
        assert_eq!(replay.due(started, started + Duration::from_millis(199)), None);
        assert_eq!(replay.due(started, started + Duration::from_millis(200)), Some(first));

        replay.delivered(first, started + Duration::from_millis(200));
        let latest = Progress {
            discovered_files: 2,
            completed_files: 1,
            ..first
        };
        replay.delivered(latest, started + Duration::from_millis(225));
        let delivered = replay.delivered(
            Progress {
                discovered_files: 1,
                completed_files: 0,
                ..Progress::default()
            },
            started + Duration::from_millis(230),
        );
        assert_eq!(delivered, latest);
        assert_eq!(replay.due(started, started + Duration::from_millis(279)), None);
        assert_eq!(replay.due(started, started + Duration::from_millis(280)), Some(latest));
    }

    #[test]
    fn progress_coalesces_when_the_bounded_event_queue_is_full() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender
            .try_send(QueuedEvent::Event(Event::Operand(operand(1))))
            .expect("queue has one slot");
        let sink = EventQueue {
            sender,
            latest_progress: Arc::new(Mutex::new(None)),
            progress_pending: Arc::new(AtomicBool::new(false)),
        };
        sink.emit(Event::Progress(Progress {
            discovered_files: 4,
            completed_files: 2,
            bytes_streamed: 8,
            ..Progress::default()
        }))
        .expect("progress is coalesced");
        sink.emit(Event::Progress(Progress {
            discovered_files: 5,
            completed_files: 3,
            logical_bytes_cloned: 13,
            ..Progress::default()
        }))
        .expect("progress is coalesced");
        assert_eq!(
            *sink.latest_progress.lock().expect("progress state is available"),
            Some(Progress {
                discovered_files: 5,
                completed_files: 3,
                logical_bytes_cloned: 13,
                bytes_streamed: 8,
                ..Progress::default()
            })
        );
    }

    #[test]
    fn terminal_outcomes_wait_for_space_instead_of_being_dropped() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(QueuedEvent::Event(Event::Operand(operand(1))))
            .expect("queue has one slot");
        let sink = EventQueue {
            sender,
            latest_progress: Arc::new(Mutex::new(None)),
            progress_pending: Arc::new(AtomicBool::new(false)),
        };
        let worker = thread::spawn(move || sink.emit(Event::Operand(operand(2))));
        assert!(matches!(receiver.recv(), Ok(QueuedEvent::Event(_))));
        assert!(matches!(
            receiver.recv(),
            Ok(QueuedEvent::Event(Event::Operand(OperandOutcome {
                state: OperandState::Moved,
                ..
            })))
        ));
        worker.join().expect("producer joined").expect("outcome delivered");
    }

    #[test]
    fn user_cancellation_takes_precedence_over_output_failure() {
        let cancellation = Cancellation::default();
        cancellation.cancel_for_output();
        assert_eq!(cancellation.reason(), Some(ish_fs::CancelReason::Output));
        cancellation.cancel();
        cancellation.cancel_for_output();
        assert_eq!(cancellation.reason(), Some(ish_fs::CancelReason::User));
    }

    #[test]
    fn run_drains_events_and_joins_a_cooperatively_cancelled_worker() {
        let mut foreground = Foreground::start().expect("foreground controller starts");
        let mut handled_operand = false;
        let result = foreground
            .run(
                |cancellation, sink| {
                    sink.emit(Event::Operand(operand(2))).expect("event is delivered");
                    while !cancellation.is_cancelled() {
                        thread::sleep(Duration::from_millis(1));
                    }
                    "stopped"
                },
                |foreground, event| {
                    let Event::Operand(_) = event else {
                        panic!("operand event is delivered");
                    };
                    handled_operand = true;
                    foreground.cancellation().cancel();
                    Ok(())
                },
            )
            .expect("worker joins after cancellation");
        assert_eq!(result, "stopped");
        assert!(handled_operand);
        assert_eq!(
            foreground.cancellation().reason(),
            Some(ish_fs::CancelReason::User)
        );
    }

    fn operand(index: u8) -> OperandOutcome {
        OperandOutcome {
            source: PathBuf::from(format!("source-{index}")),
            destination: PathBuf::from(format!("destination-{index}")),
            state: OperandState::Moved,
        }
    }
}
