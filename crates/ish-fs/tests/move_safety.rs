use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::sync::atomic::{AtomicUsize, Ordering};
use ish_fs::{Cancellation, Collision, Context, Event, Operands, Operation, Options, Request, execute};

mod support;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().join(".work/tmp"); fs::create_dir_all(&base).unwrap();
        let path = base.join(format!("move-safety-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))); fs::create_dir(&path).unwrap(); Self(path)
    }
    fn request(&self) -> Request { Request { operation: Operation::Move, operands: Operands::Exact { source: "source".into(), destination: "destination".into() }, options: Options::default(), context: Context { cwd: self.0.clone(), home: None, scratch: self.0.join("nonexistent-scratch") } } }
    fn run(&self, request: &Request) -> (i32, Vec<Event>) {
        let events = Mutex::new(Vec::new()); let outcome = execute(request, &Cancellation::default(), &|event| -> io::Result<()> { if let Event::Diagnostic(diagnostic) = &event { eprintln!("{}", diagnostic.message); } events.lock().unwrap().push(event); Ok(()) }); (outcome.status(), events.into_inner().unwrap())
    }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

#[test]
fn same_filesystem_directory_rename_does_not_walk_or_start_workers() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::write(fixture.0.join("source/file"), b"whole object").unwrap();
    let before = fs::metadata(fixture.0.join("source")).unwrap().ino();
    assert_eq!(fixture.run(&fixture.request()).0, 0);
    assert!(!fixture.0.join("source").exists()); assert_eq!(fs::metadata(fixture.0.join("destination")).unwrap().ino(), before);
    assert_eq!(fs::read(fixture.0.join("destination/file")).unwrap(), b"whole object");
}

#[test]
fn same_filesystem_special_object_moves_without_opening_contents() {
    let fixture = Fixture::new();
    support::make_fifo(&fixture.0.join("source")).unwrap();
    let before = fs::symlink_metadata(fixture.0.join("source")).unwrap().ino();
    assert_eq!(fixture.run(&fixture.request()).0, 0); assert_eq!(fs::symlink_metadata(fixture.0.join("destination")).unwrap().ino(), before);
}

#[test]
fn skip_and_same_entry_never_retire_source() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"retain").unwrap(); fs::write(fixture.0.join("destination"), b"existing").unwrap();
    let mut request = fixture.request(); request.options.collision = Collision::Skip;
    assert_eq!(fixture.run(&request).0, 0); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"retain");
    request.operands = Operands::Exact { source: "source".into(), destination: "source".into() };
    assert_eq!(fixture.run(&request).0, 0); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"retain");
}

#[test]
fn merge_renames_absent_subtrees_without_transfer() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::create_dir(fixture.0.join("source/tree")).unwrap(); fs::write(fixture.0.join("source/tree/file"), b"subtree").unwrap(); fs::create_dir(fixture.0.join("destination")).unwrap();
    let before = fs::metadata(fixture.0.join("source/tree")).unwrap().ino();
    let mut request = fixture.request(); request.options.merge = true; request.context.scratch = fixture.0.clone();
    assert_eq!(fixture.run(&request).0, 0); assert_eq!(fs::metadata(fixture.0.join("destination/tree")).unwrap().ino(), before); assert!(!fixture.0.join("source").exists());
}

#[test]
fn pre_cancelled_move_never_mutates_names() {
    let fixture = Fixture::new(); fs::write(fixture.0.join("source"), b"retain").unwrap();
    let cancellation = Cancellation::default(); cancellation.cancel();
    assert_eq!(execute(&fixture.request(), &cancellation, &|_: Event| Ok(())).status(), 130); assert_eq!(fs::read(fixture.0.join("source")).unwrap(), b"retain"); assert!(!fixture.0.join("destination").exists());
}

#[test]
fn wide_move_merge_consumes_captured_names_before_mutating_directory() {
    let fixture = Fixture::new(); fs::create_dir(fixture.0.join("source")).unwrap(); fs::create_dir(fixture.0.join("destination")).unwrap();
    for index in 0_u32..512 { fs::write(fixture.0.join(format!("source/{index}")), index.to_le_bytes()).unwrap(); }
    let mut request = fixture.request(); request.options.merge = true; request.context.scratch = fixture.0.clone();
    assert_eq!(fixture.run(&request).0, 0); assert!(!fixture.0.join("source").exists());
    for index in 0_u32..512 { assert_eq!(fs::read(fixture.0.join(format!("destination/{index}"))).unwrap(), index.to_le_bytes()); }
}
