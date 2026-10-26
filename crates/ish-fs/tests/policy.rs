use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ish_fs::{
    Cancellation, Collision, Context, Event, Operands, Operation, Options, Outcome, Request, execute,
};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    scratch: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    File,
    Directory,
    Symlink,
    DanglingSymlink,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    Transfer,
    Replace,
    Merge,
    Skip,
    Error,
}

impl Decision {
    fn status(self) -> i32 {
        if self == Self::Error { 1 } else { 0 }
    }
}

impl Fixture {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/tmp");
        std::fs::create_dir_all(&parent).expect("create fixture parent");
        let root = loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("ish-fs-policy-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        };
        let cwd = root.join("cwd");
        let scratch = root.join("scratch");
        std::fs::create_dir(&cwd).expect("create working directory");
        std::fs::create_dir(&scratch).expect("create scratch directory");
        Self { root, cwd, scratch }
    }

    fn request(&self, operation: Operation, operands: Operands, options: Options) -> Request {
        Request {
            operation,
            operands,
            options,
            context: Context { cwd: self.cwd.clone(), home: None, scratch: self.scratch.clone() },
        }
    }

    fn run(&self, request: &Request) -> Outcome {
        execute(request, &Cancellation::default(), &|_: Event| Ok(()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).expect("remove fixture");
    }
}

#[test]
fn generated_top_level_policy_matrix_matches_the_logical_model() {
    let kinds = [Kind::File, Kind::Directory, Kind::Symlink, Kind::DanglingSymlink];
    let collisions = [Collision::Error, Collision::Skip, Collision::Replace];

    for operation in [Operation::Copy, Operation::Move] {
        for source_kind in kinds {
            for destination_kind in kinds.into_iter().map(Some).chain(std::iter::once(None)) {
                for collision in collisions {
                    for merge in [false, true] {
                        let fixture = Fixture::new();
                        create_entry(&fixture.cwd, "source", source_kind, "source");
                        if let Some(destination_kind) = destination_kind {
                            create_entry(&fixture.cwd, "destination", destination_kind, "destination");
                        }
                        let decision = policy_model(source_kind, destination_kind, collision, merge);
                        let request = fixture.request(
                            operation,
                            Operands::Exact { source: "source".into(), destination: "destination".into() },
                            Options { collision, merge, ..Options::default() },
                        );

                        let outcome = fixture.run(&request);
                        assert_eq!(outcome.status(), decision.status(), "{operation:?}: {source_kind:?} -> {destination_kind:?}, {collision:?}, merge={merge}");
                        let source_remains = std::fs::symlink_metadata(fixture.cwd.join("source")).is_ok();
                        let should_retire = operation == Operation::Move && matches!(decision, Decision::Transfer | Decision::Replace | Decision::Merge);
                        assert_eq!(source_remains, !should_retire, "{operation:?}: {source_kind:?} -> {destination_kind:?}, {collision:?}, merge={merge}, {decision:?}");

                        if decision.status() == 0 {
                            assert_successful_model_result(&fixture.cwd, destination_kind, source_kind, decision);
                        } else if let Some(destination_kind) = destination_kind {
                            assert_entry_unchanged(&fixture.cwd, "destination", destination_kind, "destination");
                        } else {
                            panic!("the model rejected an absent destination");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn into_maps_multiple_sources_with_raw_names_and_trailing_slashes() {
    let fixture = Fixture::new();
    std::fs::write(fixture.cwd.join("first"), b"one").expect("write first source");
    let proposed_name = OsString::from_vec(b"raw-\xff-name".to_vec());
    let raw_name = match std::fs::write(fixture.cwd.join(&proposed_name), b"two") {
        Ok(()) => proposed_name,
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("raw-name path leg excluded: filesystem rejected malformed UTF-8");
            let fallback = OsString::from("raw-fallback");
            std::fs::write(fixture.cwd.join(&fallback), b"two").expect("write fallback source");
            fallback
        }
        Err(error) => panic!("write second source: {error}"),
    };
    std::fs::write(fixture.cwd.join("-leading"), b"three").expect("write leading dash source");
    std::fs::create_dir(fixture.cwd.join("out")).expect("create output directory");

    let request = fixture.request(
        Operation::Copy,
        Operands::Into {
            directory: "out".into(),
            first: "first///".into(),
            rest: vec![PathBuf::from(OsStr::new(&raw_name)), "-leading".into()],
        },
        Options::default(),
    );

    assert_eq!(fixture.run(&request).status(), 0);
    assert_eq!(std::fs::read(fixture.cwd.join("out/first")).expect("read first output"), b"one");
    assert_eq!(std::fs::read(fixture.cwd.join("out").join(&raw_name)).expect("read raw output"), b"two");
    assert_eq!(std::fs::read(fixture.cwd.join("out/-leading")).expect("read leading dash output"), b"three");
}

#[test]
fn into_holds_a_final_symlink_to_its_directory() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("real-output")).expect("create resolved output directory");
    symlink("real-output", fixture.cwd.join("output-link")).expect("link output directory");
    std::fs::write(fixture.cwd.join("source"), b"data").expect("write source");

    let request = fixture.request(
        Operation::Copy,
        Operands::Into { directory: "output-link".into(), first: "source".into(), rest: Vec::new() },
        Options::default(),
    );

    assert_eq!(fixture.run(&request).status(), 0);
    assert_eq!(std::fs::read(fixture.cwd.join("real-output/source")).expect("read resolved output"), b"data");
    assert_eq!(std::fs::read_link(fixture.cwd.join("output-link")).expect("read output directory link"), PathBuf::from("real-output"));
}

#[test]
fn intermediate_parent_symlinks_resolve_and_into_stays_anchored_after_swap() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("real-source")).expect("create source directory");
    std::fs::write(fixture.cwd.join("real-source/first"), b"first").expect("write first source");
    std::fs::write(fixture.cwd.join("real-source/second"), b"second").expect("write second source");
    symlink("real-source", fixture.cwd.join("source-link")).expect("link source parent");
    for directory in ["container-one", "container-two"] {
        std::fs::create_dir(fixture.cwd.join(directory)).expect("create output parent");
        std::fs::create_dir(fixture.cwd.join(directory).join("out")).expect("create output directory");
    }
    symlink("container-one", fixture.cwd.join("container-link")).expect("link output parent");
    let request = fixture.request(
        Operation::Copy,
        Operands::Into {
            directory: "container-link/out".into(),
            first: "source-link/first".into(),
            rest: vec!["source-link/second".into()],
        },
        Options::default(),
    );
    let swapped = AtomicBool::new(false);
    let outcome = execute(&request, &Cancellation::default(), &|event: Event| -> io::Result<()> {
        if let Event::Operand(operand) = event
            && operand.source == Path::new("source-link/first")
            && !swapped.swap(true, Ordering::AcqRel)
        {
            std::fs::remove_file(fixture.cwd.join("container-link"))?;
            symlink("container-two", fixture.cwd.join("container-link"))?;
        }
        Ok(())
    });

    assert!(swapped.load(Ordering::Acquire));
    assert_eq!(outcome.status(), 0);
    assert_eq!(std::fs::read(fixture.cwd.join("container-one/out/first")).expect("read first output"), b"first");
    assert_eq!(std::fs::read(fixture.cwd.join("container-one/out/second")).expect("read second output"), b"second");
    assert_eq!(std::fs::read_dir(fixture.cwd.join("container-two/out")).expect("read retargeted directory").count(), 0);
    assert_eq!(std::fs::read_link(fixture.cwd.join("container-link")).expect("read retargeted link"), PathBuf::from("container-two"));
}

#[test]
fn into_rejects_ambiguous_basenames_before_mutation() {
    for source in ["/", ".", "..", "source/.", "source/.."] {
        let fixture = Fixture::new();
        std::fs::write(fixture.cwd.join("source"), b"data").expect("write source");
        std::fs::create_dir(fixture.cwd.join("out")).expect("create output directory");
        let request = fixture.request(
            Operation::Copy,
            Operands::Into { directory: "out".into(), first: source.into(), rest: Vec::new() },
            Options::default(),
        );

        assert_eq!(fixture.run(&request).status(), 2, "source {source:?}");
        assert_eq!(std::fs::read_dir(fixture.cwd.join("out")).expect("read output directory").count(), 0);
        assert_eq!(std::fs::read(fixture.cwd.join("source")).expect("read source"), b"data");
    }
}

#[test]
fn exact_target_appends_into_an_existing_directory() {
    let fixture = Fixture::new();
    std::fs::write(fixture.cwd.join("source"), b"data").expect("write source");
    std::fs::create_dir(fixture.cwd.join("destination")).expect("create destination directory");

    let request = fixture.request(
        Operation::Copy,
        Operands::Exact { source: "source".into(), destination: "destination/".into() },
        Options::default(),
    );

    assert_eq!(fixture.run(&request).status(), 0);
    assert_eq!(
        std::fs::read(fixture.cwd.join("destination/source")).expect("read appended output"),
        b"data"
    );
    assert_eq!(std::fs::read(fixture.cwd.join("source")).expect("read source"), b"data");
}

#[test]
fn overlapping_sources_and_destinations_fail_before_mutation() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("tree")).expect("create source directory");
    std::fs::write(fixture.cwd.join("tree/leaf"), b"leaf").expect("write nested source");
    std::fs::create_dir(fixture.cwd.join("out")).expect("create output directory");
    let request = fixture.request(
        Operation::Copy,
        Operands::Into {
            directory: "out".into(),
            first: "tree".into(),
            rest: vec!["tree/leaf".into()],
        },
        Options { collision: Collision::Replace, ..Options::default() },
    );

    assert_eq!(fixture.run(&request).status(), 2);
    assert_eq!(std::fs::read_dir(fixture.cwd.join("out")).expect("read output directory").count(), 0);

    let nested_destination = fixture.request(
        Operation::Copy,
        Operands::Exact { source: "tree".into(), destination: "tree/new".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&nested_destination).status(), 2);
    assert!(!fixture.cwd.join("tree/new").exists());

    std::fs::create_dir(fixture.cwd.join("a")).expect("create first source directory");
    std::fs::write(fixture.cwd.join("a/same"), b"first").expect("write first source");
    std::fs::write(fixture.cwd.join("out/same"), b"existing").expect("write second source and output");
    let conflicting_outputs = fixture.request(
        Operation::Copy,
        Operands::Into { directory: "out".into(), first: "a/same".into(), rest: vec!["out/same".into()] },
        Options { collision: Collision::Replace, ..Options::default() },
    );
    assert_eq!(fixture.run(&conflicting_outputs).status(), 2);
    assert_eq!(std::fs::read(fixture.cwd.join("out/same")).expect("read preserved output"), b"existing");
}

#[test]
fn duplicate_into_outputs_fail_before_any_source_transfer() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("left")).expect("create left source directory");
    std::fs::create_dir(fixture.cwd.join("right")).expect("create right source directory");
    std::fs::create_dir(fixture.cwd.join("out")).expect("create output directory");
    std::fs::write(fixture.cwd.join("left/shared"), b"left source").expect("write left source");
    std::fs::write(fixture.cwd.join("right/shared"), b"right source").expect("write right source");
    let request = fixture.request(
        Operation::Copy,
        Operands::Into {
            directory: "out".into(),
            first: "left/shared".into(),
            rest: vec!["right/shared".into()],
        },
        Options { collision: Collision::Replace, ..Options::default() },
    );

    assert_eq!(fixture.run(&request).status(), 2);
    assert_eq!(std::fs::read(fixture.cwd.join("left/shared")).expect("read left source"), b"left source");
    assert_eq!(std::fs::read(fixture.cwd.join("right/shared")).expect("read right source"), b"right source");
    assert_eq!(std::fs::read_dir(fixture.cwd.join("out")).expect("read output directory").count(), 0);
}

#[test]
fn ancestry_alias_through_a_symlinked_parent_fails_before_mutation() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("tree")).expect("create source directory");
    std::fs::write(fixture.cwd.join("tree/leaf"), b"leaf").expect("write nested source");
    symlink("tree", fixture.cwd.join("tree-link")).expect("link source directory");
    std::fs::create_dir(fixture.cwd.join("out")).expect("create output directory");
    let request = fixture.request(
        Operation::Copy,
        Operands::Into {
            directory: "out".into(),
            first: "tree".into(),
            rest: vec!["tree-link/leaf".into()],
        },
        Options { collision: Collision::Replace, ..Options::default() },
    );

    assert_eq!(fixture.run(&request).status(), 2);
    assert_eq!(std::fs::read(fixture.cwd.join("tree/leaf")).expect("read nested source"), b"leaf");
    assert_eq!(std::fs::read_dir(fixture.cwd.join("out")).expect("read output directory").count(), 0);
}

#[test]
fn dry_run_collision_and_merge_policies_preserve_names_modes_and_bytes() {
    for operation in [Operation::Copy, Operation::Move] {
        for merge in [false, true] {
            for collision in [Collision::Error, Collision::Skip, Collision::Replace] {
                let fixture = Fixture::new();
                if merge {
                    std::fs::create_dir(fixture.cwd.join("source")).expect("create source directory");
                    std::fs::create_dir(fixture.cwd.join("destination")).expect("create destination directory");
                    std::fs::write(fixture.cwd.join("source/shared"), b"source collision").expect("write source collision");
                    std::fs::write(fixture.cwd.join("source/new"), b"source new").expect("write source-only leaf");
                    std::fs::write(fixture.cwd.join("destination/shared"), b"destination collision").expect("write destination collision");
                    std::fs::write(fixture.cwd.join("destination/keep"), b"destination-only").expect("write destination-only leaf");
                    std::fs::set_permissions(fixture.cwd.join("source/shared"), std::fs::Permissions::from_mode(0o640)).expect("set source file mode");
                    std::fs::set_permissions(fixture.cwd.join("source/new"), std::fs::Permissions::from_mode(0o600)).expect("set source-only mode");
                    std::fs::set_permissions(fixture.cwd.join("destination/shared"), std::fs::Permissions::from_mode(0o604)).expect("set destination file mode");
                    std::fs::set_permissions(fixture.cwd.join("destination/keep"), std::fs::Permissions::from_mode(0o444)).expect("set destination-only mode");
                    std::fs::set_permissions(fixture.cwd.join("source"), std::fs::Permissions::from_mode(0o750)).expect("set source directory mode");
                    std::fs::set_permissions(fixture.cwd.join("destination"), std::fs::Permissions::from_mode(0o710)).expect("set destination directory mode");
                } else {
                    std::fs::write(fixture.cwd.join("source"), b"source bytes").expect("write source");
                    std::fs::write(fixture.cwd.join("destination"), b"destination bytes").expect("write destination");
                    std::fs::set_permissions(fixture.cwd.join("source"), std::fs::Permissions::from_mode(0o640)).expect("set source mode");
                    std::fs::set_permissions(fixture.cwd.join("destination"), std::fs::Permissions::from_mode(0o604)).expect("set destination mode");
                }
                std::fs::write(fixture.scratch.join("sentinel"), b"scratch bytes").expect("write scratch sentinel");
                std::fs::set_permissions(fixture.scratch.join("sentinel"), std::fs::Permissions::from_mode(0o600)).expect("set scratch mode");
                let before_cwd = tree_snapshot(&fixture.cwd).expect("snapshot working directory");
                let before_scratch = tree_snapshot(&fixture.scratch).expect("snapshot scratch directory");
                let before_root = entry_names(&fixture.root).expect("snapshot fixture root");
                let missing_scratch = fixture.root.join("dry-run-scratch");
                let mut request = fixture.request(
                    operation,
                    Operands::Exact { source: "source".into(), destination: "destination".into() },
                    Options { collision, merge, dry_run: true, ..Options::default() },
                );
                request.context.scratch = missing_scratch.clone();

                let outcome = fixture.run(&request);
                let expected = if collision == Collision::Error { 1 } else { 0 };
                assert_eq!(outcome.status(), expected, "{operation:?}, merge={merge}, {collision:?}");
                assert_eq!(tree_snapshot(&fixture.cwd).expect("snapshot working directory after dry-run"), before_cwd, "{operation:?}, merge={merge}, {collision:?}");
                assert_eq!(tree_snapshot(&fixture.scratch).expect("snapshot scratch directory after dry-run"), before_scratch);
                assert_eq!(entry_names(&fixture.root).expect("snapshot fixture root after dry-run"), before_root);
                assert!(!missing_scratch.exists(), "dry-run created its missing scratch path");
            }
        }
    }
}

#[test]
fn dry_run_of_an_absent_directory_root_creates_no_destination_or_scratch() {
    for operation in [Operation::Copy, Operation::Move] {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.cwd.join("source")).expect("create source directory");
        std::fs::write(fixture.cwd.join("source/file"), b"source bytes").expect("write source file");
        std::fs::set_permissions(fixture.cwd.join("source"), std::fs::Permissions::from_mode(0o750)).expect("set source directory mode");
        std::fs::set_permissions(fixture.cwd.join("source/file"), std::fs::Permissions::from_mode(0o640)).expect("set source file mode");
        let before_cwd = tree_snapshot(&fixture.cwd).expect("snapshot working directory");
        let before_scratch = tree_snapshot(&fixture.scratch).expect("snapshot scratch directory");
        let missing_scratch = fixture.root.join("dry-run-scratch");
        let mut request = fixture.request(
            operation,
            Operands::Exact { source: "source".into(), destination: "new-tree".into() },
            Options { dry_run: true, ..Options::default() },
        );
        request.context.scratch = missing_scratch.clone();

        assert_eq!(fixture.run(&request).status(), 0, "{operation:?}");
        assert_eq!(tree_snapshot(&fixture.cwd).expect("snapshot working directory after dry-run"), before_cwd);
        assert_eq!(tree_snapshot(&fixture.scratch).expect("snapshot scratch directory after dry-run"), before_scratch);
        assert!(!fixture.cwd.join("new-tree").exists());
        assert!(!missing_scratch.exists());
    }
}

#[test]
fn merge_directory_policy_precedes_leaf_no_clobber() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.cwd.join("source")).expect("create source directory");
    std::fs::write(fixture.cwd.join("source/shared"), b"source").expect("write source leaf");
    std::fs::write(fixture.cwd.join("source/new"), b"new").expect("write new source leaf");
    std::fs::create_dir(fixture.cwd.join("destination")).expect("create destination directory");
    std::fs::write(fixture.cwd.join("destination/shared"), b"destination").expect("write destination leaf");

    let request = fixture.request(
        Operation::Copy,
        Operands::Exact { source: "source".into(), destination: "destination".into() },
        Options { collision: Collision::Skip, merge: true, ..Options::default() },
    );

    assert_eq!(fixture.run(&request).status(), 0);
    assert_eq!(std::fs::read(fixture.cwd.join("destination/shared")).expect("read colliding leaf"), b"destination");
    assert_eq!(std::fs::read(fixture.cwd.join("destination/new")).expect("read new leaf"), b"new");
}

#[test]
fn same_entry_is_a_noop_while_hardlink_aliases_follow_collision_policy() {
    let fixture = Fixture::new();
    std::fs::write(fixture.cwd.join("source"), b"data").expect("write source");
    std::fs::hard_link(fixture.cwd.join("source"), fixture.cwd.join("alias")).expect("create hardlink alias");

    let same_entry = fixture.request(
        Operation::Copy,
        Operands::Exact { source: "source".into(), destination: "source".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&same_entry).status(), 0);

    for (collision, expected) in [(Collision::Error, 1), (Collision::Skip, 0), (Collision::Replace, 1)] {
        let request = fixture.request(
            Operation::Copy,
            Operands::Exact { source: "source".into(), destination: "alias".into() },
            Options { collision, ..Options::default() },
        );
        assert_eq!(fixture.run(&request).status(), expected, "{collision:?}");
        assert_eq!(std::fs::read(fixture.cwd.join("alias")).expect("read alias"), b"data");
    }
}

#[test]
fn move_protects_working_directory_ancestors_and_allows_exact_noop() {
    let fixture = Fixture::new();
    let same_directory = fixture.request(
        Operation::Move,
        Operands::Exact { source: ".".into(), destination: ".".into() },
        Options::default(),
    );
    assert_eq!(fixture.run(&same_directory).status(), 0);

    let work = fixture.root.join("work");
    let cwd = work.join("sub");
    let scratch = fixture.root.join("scratch-move");
    std::fs::create_dir(&work).expect("create work directory");
    std::fs::create_dir(&cwd).expect("create nested working directory");
    std::fs::create_dir(&scratch).expect("create move scratch directory");
    let request = Request {
        operation: Operation::Move,
        operands: Operands::Exact { source: "..".into(), destination: "../../moved".into() },
        options: Options::default(),
        context: Context { cwd, home: None, scratch },
    };

    assert_eq!(fixture.run(&request).status(), 1);
    assert!(work.is_dir());
    assert!(!fixture.root.join("moved").exists());
}

fn create_entry(directory: &Path, name: &str, kind: Kind, label: &str) {
    let path = directory.join(name);
    match kind {
        Kind::File => std::fs::write(path, label.as_bytes()).expect("write model file"),
        Kind::Directory => {
            std::fs::create_dir(&path).expect("create model directory");
            std::fs::write(path.join(format!("{label}-child")), label.as_bytes()).expect("write model child");
        }
        Kind::Symlink => {
            let target = format!("{label}-target");
            std::fs::write(directory.join(&target), label.as_bytes()).expect("write symlink target");
            symlink(target, path).expect("create model symlink");
        }
        Kind::DanglingSymlink => symlink(format!("{label}-missing"), path).expect("create dangling symlink"),
    }
}

fn policy_model(source: Kind, destination: Option<Kind>, collision: Collision, merge: bool) -> Decision {
    let Some(destination) = destination else {
        return Decision::Transfer;
    };
    if destination == Kind::Directory && !(source == Kind::Directory && merge) {
        return Decision::Transfer;
    }
    if source == Kind::Directory && destination == Kind::Directory && merge {
        return Decision::Merge;
    }
    if collision == Collision::Skip {
        return Decision::Skip;
    }
    if collision == Collision::Replace
        && matches!((source, destination), (Kind::File, Kind::File) | (Kind::Symlink, Kind::Symlink) | (Kind::DanglingSymlink, Kind::Symlink) | (Kind::Symlink, Kind::DanglingSymlink) | (Kind::DanglingSymlink, Kind::DanglingSymlink))
    {
        return Decision::Replace;
    }
    Decision::Error
}

fn assert_successful_model_result(
    cwd: &Path,
    destination: Option<Kind>,
    source: Kind,
    decision: Decision,
) {
    if destination.is_none() {
        assert_model_entry(cwd, "destination", source, "source");
        return;
    }
    if decision == Decision::Merge {
        assert_eq!(std::fs::read(cwd.join("destination/source-child")).expect("read merged child"), b"source");
        assert_eq!(std::fs::read(cwd.join("destination/destination-child")).expect("read preserved merged child"), b"destination");
        return;
    }
    if destination == Some(Kind::Directory) && decision == Decision::Transfer {
        assert_model_entry(cwd, "destination/source", source, "source");
        assert_eq!(
            std::fs::read(cwd.join("destination/destination-child")).expect("read preserved destination child"),
            b"destination"
        );
        return;
    }
    if decision == Decision::Replace {
        assert_model_entry(cwd, "destination", source, "source");
    } else if decision == Decision::Skip {
        assert_entry_unchanged(cwd, "destination", destination.unwrap(), "destination");
    } else {
        panic!("an absent destination must transfer");
    }
}

fn assert_model_entry(cwd: &Path, name: &str, kind: Kind, label: &str) {
    let path = cwd.join(name);
    match kind {
        Kind::File => assert_eq!(std::fs::read(path).expect("read model output"), label.as_bytes()),
        Kind::Directory => assert_eq!(std::fs::read(path.join(format!("{label}-child"))).expect("read model directory output"), label.as_bytes()),
        Kind::Symlink => {
            assert_eq!(std::fs::read_link(path).expect("read model link output"), PathBuf::from(format!("{label}-target")));
            assert_eq!(std::fs::read(cwd.join(format!("{label}-target"))).expect("read model link referent"), label.as_bytes());
        }
        Kind::DanglingSymlink => assert_eq!(std::fs::read_link(path).expect("read dangling model link output"), PathBuf::from(format!("{label}-missing"))),
    }
}

fn assert_entry_unchanged(cwd: &Path, name: &str, kind: Kind, label: &str) {
    let path = cwd.join(name);
    match kind {
        Kind::File => assert_eq!(std::fs::read(path).expect("read preserved model file"), label.as_bytes()),
        Kind::Directory => assert_eq!(std::fs::read(path.join(format!("{label}-child"))).expect("read preserved model child"), label.as_bytes()),
        Kind::Symlink => {
            assert_eq!(std::fs::read_link(path).expect("read preserved model link"), PathBuf::from(format!("{label}-target")));
            assert_eq!(std::fs::read(cwd.join(format!("{label}-target"))).expect("read preserved link referent"), label.as_bytes());
        }
        Kind::DanglingSymlink => assert_eq!(std::fs::read_link(path).expect("read preserved dangling link"), PathBuf::from(format!("{label}-missing"))),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SnapshotEntry {
    path: PathBuf,
    mode: u32,
    identity: (u64, u64),
    ownership: (u32, u32),
    modified: (i64, i64),
    changed: (i64, i64),
    kind: SnapshotKind,
    bytes: Option<Vec<u8>>,
    link_target: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SnapshotKind {
    File,
    Directory,
    Symlink,
    Other,
}

fn tree_snapshot(root: &Path) -> io::Result<Vec<SnapshotEntry>> {
    fn append(root: &Path, relative: &Path, snapshot: &mut Vec<SnapshotEntry>) -> io::Result<()> {
        let directory = root.join(relative);
        let mut children = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let relative_path = relative.join(child.file_name());
            let path = root.join(&relative_path);
            let metadata = std::fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_file() {
                SnapshotKind::File
            } else if file_type.is_dir() {
                SnapshotKind::Directory
            } else if file_type.is_symlink() {
                SnapshotKind::Symlink
            } else {
                SnapshotKind::Other
            };
            snapshot.push(SnapshotEntry {
                path: relative_path.clone(),
                mode: metadata.mode() & 0o7777,
                identity: (metadata.dev(), metadata.ino()),
                ownership: (metadata.uid(), metadata.gid()),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
                bytes: if kind == SnapshotKind::File { Some(std::fs::read(&path)?) } else { None },
                link_target: if kind == SnapshotKind::Symlink { Some(std::fs::read_link(&path)?) } else { None },
                kind: kind.clone(),
            });
            if kind == SnapshotKind::Directory {
                append(root, &relative_path, snapshot)?;
            }
        }
        Ok(())
    }

    let metadata = std::fs::symlink_metadata(root)?;
    let mut snapshot = vec![SnapshotEntry {
        path: PathBuf::from("."),
        mode: metadata.mode() & 0o7777,
        identity: (metadata.dev(), metadata.ino()),
        ownership: (metadata.uid(), metadata.gid()),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        kind: SnapshotKind::Directory,
        bytes: None,
        link_target: None,
    }];
    append(root, Path::new(""), &mut snapshot)?;
    snapshot.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(snapshot)
}

fn entry_names(directory: &Path) -> io::Result<Vec<OsString>> {
    let mut names = std::fs::read_dir(directory)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

#[test]
fn exact_into_existing_directory_respects_collision_policy() {
    for (collision, expected_status, should_move) in [
        (Collision::Error, 1, false),
        (Collision::Skip, 0, false),
        (Collision::Replace, 0, true),
    ] {
        let fixture = Fixture::new();
        std::fs::write(fixture.cwd.join("a"), b"new").expect("write source");
        std::fs::create_dir(fixture.cwd.join("b")).expect("create destination directory");
        std::fs::write(fixture.cwd.join("b/a"), b"existing").expect("write conflicting child");
        let request = fixture.request(
            Operation::Move,
            Operands::Exact { source: "a".into(), destination: "b".into() },
            Options { collision, ..Options::default() },
        );
        assert_eq!(fixture.run(&request).status(), expected_status, "{collision:?}");
        if should_move {
            assert_eq!(std::fs::read(fixture.cwd.join("b/a")).expect("read replaced"), b"new");
            assert!(!fixture.cwd.join("a").exists());
        } else {
            assert_eq!(std::fs::read(fixture.cwd.join("b/a")).expect("read preserved"), b"existing");
            assert!(fixture.cwd.join("a").exists());
        }
    }
}
