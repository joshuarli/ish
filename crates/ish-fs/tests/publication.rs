use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use ish_fs::{Cancellation, Collision, Context, Event, Operands, Operation, Options, Request, execute};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    scratch: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap();
        let parent = workspace.join(".work/tmp");
        fs::create_dir_all(&parent).unwrap();
        let root = loop {
            let index = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("publication-{}-{index}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        };
        let cwd = root.join("cwd");
        let scratch = root.join("scratch");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir(&scratch).unwrap();
        Self { root, cwd, scratch }
    }

    fn request(&self, operands: Operands, options: Options) -> Request {
        Request {
            operation: Operation::Copy,
            operands,
            options,
            context: Context { cwd: self.cwd.clone(), home: None, scratch: self.scratch.clone() },
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn set_times(path: &Path, seconds: i64, nanoseconds: i64) {
    let file = OpenOptions::new().read(true).write(true).open(path).unwrap();
    rustix::fs::futimens(
        &file,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
            last_modification: rustix::fs::Timespec { tv_sec: seconds, tv_nsec: nanoseconds },
        },
    )
    .unwrap();
}

#[test]
fn replace_transfers_same_size_same_mtime_drift_while_no_clobber_skips_it() {
    let fixture = Fixture::new();
    fs::write(fixture.cwd.join("replace-source"), b"new bytes").unwrap();
    fs::write(fixture.cwd.join("replace-target"), b"old bytes").unwrap();
    fs::write(fixture.cwd.join("skip-source"), b"new bytes").unwrap();
    fs::write(fixture.cwd.join("skip-target"), b"old bytes").unwrap();
    for name in ["replace-source", "replace-target", "skip-source", "skip-target"] {
        set_times(&fixture.cwd.join(name), 1_700_000_500, 321_000_000);
    }
    for pair in [["replace-source", "replace-target"], ["skip-source", "skip-target"]] {
        let source = fs::metadata(fixture.cwd.join(pair[0])).unwrap();
        let destination = fs::metadata(fixture.cwd.join(pair[1])).unwrap();
        assert_eq!(source.len(), destination.len());
        assert_eq!((source.mtime(), source.mtime_nsec()), (destination.mtime(), destination.mtime_nsec()));
    }

    let replace = fixture.request(
        Operands::Exact { source: "replace-source".into(), destination: "replace-target".into() },
        Options { collision: Collision::Replace, ..Options::default() },
    );
    assert_eq!(execute(&replace, &Cancellation::default(), &|_: Event| Ok(())).status(), 0);
    assert_eq!(fs::read(fixture.cwd.join("replace-target")).unwrap(), b"new bytes");

    let skip = fixture.request(
        Operands::Exact { source: "skip-source".into(), destination: "skip-target".into() },
        Options { collision: Collision::Skip, ..Options::default() },
    );
    let outcome = execute(&skip, &Cancellation::default(), &|_: Event| Ok(()));
    assert_eq!(outcome.status(), 0);
    assert_eq!(outcome.progress.skipped_files, 1);
    assert_eq!(fs::read(fixture.cwd.join("skip-target")).unwrap(), b"old bytes");
}

#[test]
fn destination_created_after_preflight_survives_for_later_into_operand() {
    for late_symlink in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.cwd.join("left")).unwrap();
        fs::create_dir(fixture.cwd.join("right")).unwrap();
        fs::create_dir(fixture.cwd.join("out")).unwrap();
        fs::write(fixture.cwd.join("left/first"), b"first source").unwrap();
        fs::write(fixture.cwd.join("right/second"), b"second source").unwrap();
        fs::write(fixture.cwd.join("referent"), b"late destination").unwrap();
        let injected = AtomicBool::new(false);
        let request = fixture.request(
            Operands::Into {
                directory: "out".into(),
                first: "left/first".into(),
                rest: vec!["right/second".into()],
            },
            Options::default(),
        );

        let outcome = execute(&request, &Cancellation::default(), &|event: Event| -> io::Result<()> {
            if let Event::Operand(operand) = event
                && operand.source.as_path() == Path::new("left/first") && !injected.swap(true, Ordering::AcqRel)
            {
                if late_symlink {
                    std::os::unix::fs::symlink("../referent", fixture.cwd.join("out/second"))?;
                } else {
                    fs::write(fixture.cwd.join("out/second"), b"late destination")?;
                }
            }
            Ok(())
        });

        assert!(injected.load(Ordering::Acquire));
        assert_eq!(outcome.status(), 1);
        assert_eq!(outcome.progress.failed_files, 1);
        assert_eq!(fs::read(fixture.cwd.join("out/first")).unwrap(), b"first source");
        assert_eq!(fs::read(fixture.cwd.join("out/second")).unwrap(), b"late destination");
        assert_eq!(fs::read(fixture.cwd.join("referent")).unwrap(), b"late destination");
        if late_symlink {
            assert_eq!(fs::read_link(fixture.cwd.join("out/second")).unwrap(), Path::new("../referent"));
        }
        assert_eq!(fs::read(fixture.cwd.join("right/second")).unwrap(), b"second source");
        assert_eq!(fs::read_dir(fixture.cwd.join("out")).unwrap().count(), 2);
    }
}

#[test]
fn concurrent_readers_observe_complete_old_or_new_replacement_bytes() {
    let fixture = Fixture::new();
    let source = fixture.cwd.join("source");
    let destination = fixture.cwd.join("destination");
    let old = Arc::new(vec![b'o'; 16 * 1024 * 1024]);
    let new = Arc::new((0..16 * 1024 * 1024).map(|index| ((index * 29 + 17) % 251) as u8).collect::<Vec<_>>());
    fs::write(&source, new.as_slice()).unwrap();
    fs::write(&destination, old.as_slice()).unwrap();
    let request = fixture.request(
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options { collision: Collision::Replace, ..Options::default() },
    );
    let done = Arc::new(AtomicBool::new(false));
    let invalid = Arc::new(AtomicBool::new(false));
    let snapshots = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let (first_snapshot, first_snapshot_received) = mpsc::sync_channel(0);
    let reader_path = destination.clone();
    let reader_old = Arc::clone(&old);
    let reader_new = Arc::clone(&new);
    let reader_done = Arc::clone(&done);
    let reader_invalid = Arc::clone(&invalid);
    let reader_snapshots = Arc::clone(&snapshots);
    let reader_barrier = Arc::clone(&barrier);
    let reader = thread::spawn(move || {
        reader_barrier.wait();
        let mut first = true;
        while !reader_done.load(Ordering::Acquire) {
            match fs::read(&reader_path) {
                Ok(bytes) if bytes.as_slice() == reader_old.as_slice() || bytes.as_slice() == reader_new.as_slice() => {
                    reader_snapshots.fetch_add(1, Ordering::Relaxed);
                }
                _ => reader_invalid.store(true, Ordering::Release),
            }
            if first {
                first_snapshot.send(()).unwrap();
                first = false;
            }
            thread::yield_now();
        }
    });
    barrier.wait();
    first_snapshot_received.recv().unwrap();

    let outcome = execute(&request, &Cancellation::default(), &|_: Event| Ok(()));
    done.store(true, Ordering::Release);
    reader.join().unwrap();

    assert_eq!(outcome.status(), 0);
    assert!(!invalid.load(Ordering::Acquire), "reader observed a missing or partial destination");
    assert!(snapshots.load(Ordering::Relaxed) > 0);
    assert_eq!(fs::read(destination).unwrap(), new.as_slice());
}
