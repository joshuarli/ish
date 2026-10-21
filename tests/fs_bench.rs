use ish_fs::{
    Cancellation, Context, Event, Jobs, Metrics, Operands, Operation, Options, Progress, Request,
    MAX_DESCRIPTORS, MIN_DESCRIPTORS, QUEUE_CAPACITY, RECORD_MEMORY_BYTES,
    RESERVED_DESCRIPTORS, TRANSFER_BUFFER_BYTES,
};
use ptytest::{CommandSpec, ProtocolProfile, PtyTest, Scenario, Size, TestEnv};
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const MOVE_ITERATIONS: usize = 32;
const MAX_SPARSE_ALLOCATED_BYTES: u64 = 16 * 1024 * 1024;
const RUNS_DIRECTORY: &str = ".ish-fs-bench-runs";
const RUN_MARKER: &str = ".ish-fs-bench-run-owner";

#[test]
#[ignore = "explicit fs benchmark sample runner"]
fn fs_bench_sample() {
    let config = Config::from_environment();
    let prepared = Prepared::open(&config);
    match config.side {
        Side::Engine => sample_engine(&config, &prepared),
        Side::Builtin => sample_builtin(&config, &prepared),
        Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
            sample_native(&config, &prepared)
        }
    }
}

#[test]
#[ignore = "explicit fs benchmark fixture preparation"]
fn fs_bench_prepare_sample() {
    let config = Config::from_environment();
    let prepared = Prepared::create(&config);
    println!(
        "fs_bench_fixture_prepared workload={} side={} sample={} inputs={}",
        config.workload.name(),
        config.side.name(),
        config.sample,
        prepared.expected_count()
    );
}

#[test]
#[ignore = "explicit fs benchmark output verifier"]
fn fs_bench_verify() {
    let config = Config::from_environment();
    let prepared = Prepared::open(&config);
    prepared.verify(&config);
    println!(
        "fs_bench_verified workload={} side={} sample={} outputs={}",
        config.workload.name(),
        config.side.name(),
        config.sample,
        prepared.expected_count()
    );
}

#[test]
#[ignore = "explicit fs benchmark sample cleanup"]
fn fs_bench_cleanup() {
    let config = Config::from_environment();
    let prepared = Prepared::open(&config);
    prepared.cleanup(&config);
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Side {
    Engine,
    Builtin,
    NativeCopy,
    NativeClone,
    NativeMove,
}

impl Side {
    fn parse(value: &str) -> Self {
        match value {
            "engine" => Self::Engine,
            "builtin" => Self::Builtin,
            "native-cp" => Self::NativeCopy,
            "native-clone" => Self::NativeClone,
            "native-mv" => Self::NativeMove,
            _ => panic!("unknown ISH_FS_BENCH_SIDE={value}"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Builtin => "builtin",
            Self::NativeCopy => "native-cp",
            Self::NativeClone => "native-clone",
            Self::NativeMove => "native-mv",
        }
    }

    fn native_program(self) -> Option<OsString> {
        match self {
            Self::NativeCopy | Self::NativeClone => Some(
                env::var_os("ISH_FS_BENCH_CP").unwrap_or_else(|| OsString::from("cp")),
            ),
            Self::NativeMove => Some(
                env::var_os("ISH_FS_BENCH_MV").unwrap_or_else(|| OsString::from("mv")),
            ),
            Self::Engine | Self::Builtin => None,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Workload {
    TinyCopy,
    TinyMove,
    Flat,
    Nested,
    Deep,
    Wide,
    Large,
    Clone,
    Sparse,
    Hardlinks,
    Rename,
    Cross,
    Merge,
    ProgressOn,
    ProgressOff,
}

impl Workload {
    fn parse(value: &str) -> Self {
        match value {
            "tiny-copy" => Self::TinyCopy,
            "tiny-move" => Self::TinyMove,
            "flat" => Self::Flat,
            "nested" => Self::Nested,
            "deep" => Self::Deep,
            "wide" => Self::Wide,
            "large" => Self::Large,
            "clone" => Self::Clone,
            "sparse" => Self::Sparse,
            "hardlinks" => Self::Hardlinks,
            "rename" => Self::Rename,
            "cross" => Self::Cross,
            "merge" => Self::Merge,
            "progress-on" => Self::ProgressOn,
            "progress-off" => Self::ProgressOff,
            _ => panic!("unknown ISH_FS_BENCH_WORKLOAD={value}"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::TinyCopy => "tiny-copy",
            Self::TinyMove => "tiny-move",
            Self::Flat => "flat",
            Self::Nested => "nested",
            Self::Deep => "deep",
            Self::Wide => "wide",
            Self::Large => "large",
            Self::Clone => "clone",
            Self::Sparse => "sparse",
            Self::Hardlinks => "hardlinks",
            Self::Rename => "rename",
            Self::Cross => "cross",
            Self::Merge => "merge",
            Self::ProgressOn => "progress-on",
            Self::ProgressOff => "progress-off",
        }
    }

    fn is_move(self) -> bool {
        matches!(self, Self::TinyMove | Self::Rename | Self::Cross)
    }

    fn constrained(self) -> bool {
        matches!(self, Self::Deep | Self::Wide)
    }

    fn terminal_progress(self) -> Option<bool> {
        match self {
            Self::ProgressOn => Some(true),
            Self::ProgressOff => Some(false),
            _ => None,
        }
    }
}

struct Config {
    root: PathBuf,
    identity: (u64, u64),
    cross_root: Option<PathBuf>,
    cross_identity: Option<(u64, u64)>,
    workload: Workload,
    side: Side,
    sample: String,
}

impl Config {
    fn from_environment() -> Self {
        let root = fs::canonicalize(PathBuf::from(required("ISH_FS_BENCH_ROOT")))
            .expect("canonicalize fixture root");
        let identity = parse_identity(&required("ISH_FS_BENCH_IDENTITY"));
        let cross_root = env::var_os("ISH_FS_BENCH_CROSS_ROOT")
            .map(PathBuf::from)
            .map(|path| fs::canonicalize(path).expect("canonicalize cross-filesystem root"));
        let cross_identity = env::var("ISH_FS_BENCH_CROSS_IDENTITY")
            .ok()
            .map(|value| parse_identity(&value));
        if cross_root.is_some() != cross_identity.is_some() {
            panic!("cross root and identity must be supplied together");
        }
        let config = Self {
            root,
            identity,
            cross_root,
            cross_identity,
            workload: Workload::parse(&required("ISH_FS_BENCH_WORKLOAD")),
            side: Side::parse(&required("ISH_FS_BENCH_SIDE")),
            sample: required("ISH_FS_BENCH_SAMPLE"),
        };
        validate_owned_root(&config.root, config.identity);
        if let (Some(root), Some(identity)) = (&config.cross_root, config.cross_identity) {
            validate_cross_root(root, identity, config.identity);
        }
        validate_sample_name(&config.sample);
        if config.workload == Workload::Cross && config.cross_root.is_none() {
            panic!("cross workload requires an explicitly prepared cross-filesystem root");
        }
        if config.workload.is_move() != (config.side == Side::NativeMove)
            && matches!(config.side, Side::NativeCopy | Side::NativeClone | Side::NativeMove)
        {
            panic!("native cp and native mv sides do not match workload operation");
        }
        config
    }
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn parse_identity(value: &str) -> (u64, u64) {
    let (device, inode) = value
        .split_once(':')
        .unwrap_or_else(|| panic!("invalid device:inode identity {value:?}"));
    (
        device.parse().expect("identity device is a decimal integer"),
        inode.parse().expect("identity inode is a decimal integer"),
    )
}

fn validate_sample_name(sample: &str) {
    assert!(!sample.is_empty() && sample.len() <= 64);
    assert!(sample.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
    }));
}

fn expected_temp_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::canonicalize(manifest.join(".work/tmp")).expect("canonicalize designated temp root")
}

fn validate_owned_root(root: &Path, expected: (u64, u64)) {
    let root = fs::canonicalize(root).expect("canonicalize fixture root");
    assert_eq!(
        root.parent(),
        Some(expected_temp_root().as_path()),
        "fixture root must be a direct child of the designated .work/tmp"
    );
    let metadata = fs::symlink_metadata(&root).expect("stat fixture root");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!((metadata.dev(), metadata.ino()), expected);
    verify_marker(&root, expected);
}

fn validate_cross_root(root: &Path, expected: (u64, u64), owner: (u64, u64)) {
    let root = fs::canonicalize(root).expect("canonicalize cross-filesystem fixture root");
    assert!(root.starts_with(expected_temp_root()));
    let metadata = fs::symlink_metadata(&root).expect("stat cross-filesystem fixture root");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!((metadata.dev(), metadata.ino()), expected);
    assert_ne!(expected.0, owner.0, "cross root must be on a distinct device");
    let marker = fs::read_to_string(root.join(".ish-fs-bench-owner"))
        .expect("read cross-filesystem fixture marker");
    assert!(marker.lines().any(|line| line == format!("device={}", expected.0)));
    assert!(marker.lines().any(|line| line == format!("inode={}", expected.1)));
    assert!(marker.lines().any(|line| line == format!("owner_device={}", owner.0)));
    assert!(marker.lines().any(|line| line == format!("owner_inode={}", owner.1)));
}

fn verify_marker(root: &Path, expected: (u64, u64)) {
    let marker = fs::read_to_string(root.join(".ish-fs-bench-owner"))
        .expect("read fixture ownership marker");
    assert!(marker.lines().any(|line| line == "format=1"));
    assert!(marker.lines().any(|line| line == format!("device={}", expected.0)));
    assert!(marker.lines().any(|line| line == format!("inode={}", expected.1)));
}

fn ensure_run_parent(root: &Path, expected: (u64, u64)) {
    let parent = root.join(RUNS_DIRECTORY);
    match fs::create_dir(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("create sample parent: {error}"),
    }
    let metadata = fs::symlink_metadata(&parent).expect("stat sample parent");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.dev(), expected.0);
    assert_eq!(fs::canonicalize(&parent).expect("canonicalize sample parent"), parent);
}

fn write_run_marker(path: &Path, parent: (u64, u64), fixture: (u64, u64)) {
    let metadata = fs::symlink_metadata(path).expect("stat new sample directory");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    let contents = format!(
        "format=1\nparent_device={}\nparent_inode={}\nfixture_device={}\nfixture_inode={}\nrun_device={}\nrun_inode={}\n",
        parent.0,
        parent.1,
        fixture.0,
        fixture.1,
        metadata.dev(),
        metadata.ino()
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path.join(RUN_MARKER))
        .expect("create sample ownership marker")
        .write_all(contents.as_bytes())
        .expect("write sample ownership marker");
}

fn verify_run_marker(path: &Path, parent: (u64, u64), fixture: (u64, u64)) {
    let parent_path = path.parent().expect("sample directory parent");
    let parent_metadata = fs::symlink_metadata(parent_path).expect("stat sample parent");
    assert!(parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink());
    assert_eq!(parent_metadata.dev(), parent.0);
    assert_eq!(fs::canonicalize(parent_path).expect("canonicalize sample parent"), parent_path);
    let metadata = fs::symlink_metadata(path).expect("stat sample directory");
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    let marker = fs::read_to_string(path.join(RUN_MARKER)).expect("read sample ownership marker");
    for expected_line in [
        "format=1".to_owned(),
        format!("parent_device={}", parent.0),
        format!("parent_inode={}", parent.1),
        format!("fixture_device={}", fixture.0),
        format!("fixture_inode={}", fixture.1),
        format!("run_device={}", metadata.dev()),
        format!("run_inode={}", metadata.ino()),
    ] {
        assert!(marker.lines().any(|line| line == expected_line));
    }
}

#[derive(Clone)]
struct Transfer {
    source: PathBuf,
    destination: PathBuf,
    expected: PathBuf,
    merge: bool,
    cross_file_systems: bool,
}

struct Prepared {
    sample_root: PathBuf,
    transfers: Vec<Transfer>,
}

impl Prepared {
    fn create(config: &Config) -> Self {
        let sample_root = config.root.join(RUNS_DIRECTORY).join(&config.sample);
        ensure_run_parent(&config.root, config.identity);
        fs::create_dir(&sample_root).expect("create unique sample directory");
        write_run_marker(&sample_root, config.identity, config.identity);
        fs::create_dir(sample_root.join("scratch")).expect("create engine scratch directory");
        let transfers = prepare_transfers(config, &sample_root);
        Self {
            sample_root,
            transfers,
        }
    }

    fn open(config: &Config) -> Self {
        let sample_root = config.root.join(RUNS_DIRECTORY).join(&config.sample);
        verify_run_marker(&sample_root, config.identity, config.identity);
        if config.workload == Workload::Cross {
            let cross_root = config.cross_root.as_ref().expect("cross root");
            let cross_identity = config.cross_identity.expect("cross identity");
            verify_run_marker(
                &cross_root.join(RUNS_DIRECTORY).join(&config.sample),
                cross_identity,
                config.identity,
            );
        }
        let transfers = expected_transfers(config, &sample_root, false);
        Self {
            sample_root,
            transfers,
        }
    }

    fn expected_count(&self) -> usize {
        self.transfers.len()
    }

    fn verify(&self, config: &Config) {
        for transfer in &self.transfers {
            if transfer.merge {
                verify_merged_tree(
                    &config.root.join("merge/source"),
                    &config.root.join("merge/destination"),
                    &transfer.destination,
                );
            } else if config.workload == Workload::Sparse {
                verify_sparse_copy(
                    &transfer.expected,
                    &transfer.destination,
                    matches!(config.side, Side::Engine | Side::Builtin),
                );
            } else {
                compare_tree(&transfer.expected, &transfer.destination);
            }
            if config.workload.is_move() {
                match fs::symlink_metadata(&transfer.source) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => panic!("inspect moved source entry: {error}"),
                    Ok(_) => panic!("move left its source entry behind"),
                }
            }
        }
        if config.workload == Workload::Hardlinks
            && matches!(config.side, Side::Engine | Side::Builtin)
        {
            let destination = &self.transfers[0].destination;
            let first = fs::metadata(destination.join("first")).unwrap();
            let alias = fs::metadata(destination.join("alias")).unwrap();
            assert_eq!((first.dev(), first.ino()), (alias.dev(), alias.ino()));
        }
        if config.workload == Workload::Rename && config.side == Side::Engine {
            assert_eq!(self.transfers.len(), 1);
        }
    }

    fn cleanup(&self, config: &Config) {
        validate_owned_root(&config.root, config.identity);
        verify_run_marker(&self.sample_root, config.identity, config.identity);
        let cross_sample = if config.workload == Workload::Cross {
            let root = config.cross_root.as_ref().expect("cross root");
            let identity = config.cross_identity.expect("cross identity");
            Some((root.join(RUNS_DIRECTORY).join(&config.sample), identity))
        } else {
            None
        };
        if let Some((path, identity)) = &cross_sample {
            verify_run_marker(path, *identity, config.identity);
        }
        fs::remove_dir_all(&self.sample_root).expect("remove verified sample directory");
        if let Some((path, _)) = cross_sample {
            fs::remove_dir_all(path).expect("remove verified cross-filesystem sample directory");
        }
    }
}

fn prepare_transfers(config: &Config, sample_root: &Path) -> Vec<Transfer> {
    let source_root = &config.root;
    match config.workload {
        Workload::TinyCopy => {
            let destination = sample_root.join("destination");
            fs::create_dir(&destination).expect("create tiny-copy destination");
            (0..MOVE_ITERATIONS)
                .map(|index| Transfer {
                    source: source_root
                        .join("tiny/source")
                        .join(format!("item-{index:03}")),
                    destination: destination.join(format!("item-{index:03}")),
                    expected: source_root
                        .join("tiny/source")
                        .join(format!("item-{index:03}")),
                    merge: false,
                    cross_file_systems: false,
                })
                .collect()
        }
        Workload::TinyMove => {
            let source = sample_root.join("source");
            let destination = sample_root.join("destination");
            fs::create_dir(&source).expect("create tiny-move source");
            fs::create_dir(&destination).expect("create tiny-move destination");
            (0..MOVE_ITERATIONS)
                .map(|index| {
                    let item = format!("item-{index:03}");
                    let expected = source_root.join("tiny/source").join(&item);
                    let input = source.join(&item);
                    fs::copy(&expected, &input).expect("prepare tiny-move source copy");
                    Transfer {
                        source: input,
                        destination: destination.join(&item),
                        expected,
                        merge: false,
                        cross_file_systems: false,
                    }
                })
                .collect()
        }
        Workload::Rename => {
            let expected = source_root.join("same-fs-move/source");
            let source = sample_root.join("source");
            copy_tree(&expected, &source);
            let destination_parent = sample_root.join("destination-parent");
            fs::create_dir(&destination_parent).expect("create same-filesystem move parent");
            vec![Transfer {
                source,
                destination: destination_parent.join("renamed"),
                expected,
                merge: false,
                cross_file_systems: false,
            }]
        }
        Workload::Cross => {
            let cross_root = config.cross_root.as_ref().expect("cross root");
            let expected = source_root.join("cross-source/source/payload");
            let source = sample_root.join("cross-source").join("payload");
            fs::create_dir(source.parent().unwrap()).expect("create local cross source parent");
            fs::copy(&expected, &source).expect("prepare local source for cross-filesystem move");
            let destination_parent = cross_root.join(RUNS_DIRECTORY).join(&config.sample);
            let cross_identity = config.cross_identity.expect("cross identity");
            ensure_run_parent(cross_root, cross_identity);
            fs::create_dir(&destination_parent).expect("create unique cross-filesystem sample directory");
            write_run_marker(&destination_parent, cross_identity, config.identity);
            vec![Transfer {
                source,
                destination: destination_parent.join("payload"),
                expected,
                merge: false,
                cross_file_systems: true,
            }]
        }
        Workload::Merge => {
            let expected = source_root.join("merge/source");
            let destination = sample_root.join("destination");
            copy_tree(&source_root.join("merge/destination"), &destination);
            vec![Transfer {
                source: expected.clone(),
                destination,
                expected,
                merge: true,
                cross_file_systems: false,
            }]
        }
        Workload::Flat
        | Workload::Nested
        | Workload::Deep
        | Workload::Wide
        | Workload::Hardlinks
        | Workload::Large
        | Workload::Clone
        | Workload::Sparse
        | Workload::ProgressOn
        | Workload::ProgressOff => expected_transfers(config, sample_root, true),
    }
}

fn expected_transfers(config: &Config, sample_root: &Path, create_parent: bool) -> Vec<Transfer> {
    if config.workload == Workload::TinyCopy {
        return (0..MOVE_ITERATIONS)
            .map(|index| {
                let expected = config
                    .root
                    .join("tiny/source")
                    .join(format!("item-{index:03}"));
                Transfer {
                    source: expected.clone(),
                    destination: sample_root
                        .join("destination")
                        .join(format!("item-{index:03}")),
                    expected,
                    merge: false,
                    cross_file_systems: false,
                }
            })
            .collect();
    }
    if config.workload == Workload::TinyMove {
        return (0..MOVE_ITERATIONS)
            .map(|index| {
                let item = format!("item-{index:03}");
                Transfer {
                    source: sample_root.join("source").join(&item),
                    destination: sample_root.join("destination").join(&item),
                    expected: config.root.join("tiny/source").join(item),
                    merge: false,
                    cross_file_systems: false,
                }
            })
            .collect();
    }
    if config.workload == Workload::Rename {
        return vec![Transfer {
            source: sample_root.join("source"),
            destination: sample_root.join("destination-parent/renamed"),
            expected: config.root.join("same-fs-move/source"),
            merge: false,
            cross_file_systems: false,
        }];
    }
    if config.workload == Workload::Cross {
        let cross_root = config.cross_root.as_ref().expect("cross root");
        return vec![Transfer {
            source: sample_root.join("cross-source/payload"),
            destination: cross_root
                .join(RUNS_DIRECTORY)
                .join(&config.sample)
                .join("payload"),
            expected: config.root.join("cross-source/source/payload"),
            merge: false,
            cross_file_systems: true,
        }];
    }
    if config.workload == Workload::Merge {
        return vec![Transfer {
            source: config.root.join("merge/source"),
            destination: sample_root.join("destination"),
            expected: config.root.join("merge/source"),
            merge: true,
            cross_file_systems: false,
        }];
    }
    let (source, destination) = match config.workload {
        Workload::Flat => ("flat/source", "destination/source"),
        Workload::Nested => ("nested/source", "destination/source"),
        Workload::Deep => ("deep/source", "destination/source"),
        Workload::Wide | Workload::ProgressOn | Workload::ProgressOff => {
            ("wide/source", "destination/source")
        }
        Workload::Large => ("large/source/payload", "destination/payload"),
        Workload::Clone => ("large/source/payload", "destination/payload"),
        Workload::Sparse => ("sparse/source/payload", "destination/payload"),
        Workload::Hardlinks => ("hardlinks/source", "destination/source"),
        Workload::TinyCopy
        | Workload::TinyMove
        | Workload::Rename
        | Workload::Cross
        | Workload::Merge => unreachable!("special transfer fixture"),
    };
    let source = config.root.join(source);
    let destination = sample_root.join(destination);
    if create_parent {
        fs::create_dir_all(destination.parent().unwrap())
            .expect("create sample destination parent");
    }
    vec![Transfer {
        source: source.clone(),
        destination,
        expected: source,
        merge: false,
        cross_file_systems: false,
    }]
}

fn copy_tree(source: &Path, destination: &Path) {
    let metadata = fs::symlink_metadata(source).expect("stat fixture tree entry");
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source).expect("read fixture symlink");
        std::os::unix::fs::symlink(target, destination).expect("copy fixture symlink");
    } else if metadata.is_dir() {
        fs::create_dir(destination).expect("create copied fixture directory");
        for entry in fs::read_dir(source).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        }
    } else {
        fs::copy(source, destination).expect("copy fixture file");
    }
}

fn options_for(transfer: &Transfer, workload: Workload) -> Options {
    let mut options = Options {
        merge: transfer.merge,
        cross_file_systems: transfer.cross_file_systems,
        jobs: Some(Jobs::new(jobs_for(workload)).expect("fixed benchmark worker count")),
        ..Options::default()
    };
    if workload.is_move() {
        options.durable = true;
    }
    options
}

fn jobs_for(workload: Workload) -> usize {
    if workload.constrained() { 2 } else { 4 }
}

fn request_for(config: &Config, prepared: &Prepared, transfer: &Transfer) -> Request {
    Request {
        operation: if config.workload.is_move() {
            Operation::Move
        } else {
            Operation::Copy
        },
        operands: Operands::Exact {
            source: transfer.source.clone(),
            destination: transfer.destination.clone(),
        },
        options: options_for(transfer, config.workload),
        context: Context {
            cwd: config.root.clone(),
            home: None,
            scratch: prepared.sample_root.join("scratch"),
        },
    }
}

#[derive(Default)]
struct Totals {
    progress: Progress,
    metrics: Metrics,
    requests: usize,
}

impl Totals {
    fn add(&mut self, outcome: ish_fs::Outcome) {
        assert_eq!(outcome.status(), 0, "engine sample request failed");
        self.progress.discovered_files += outcome.progress.discovered_files;
        self.progress.completed_files += outcome.progress.completed_files;
        self.progress.skipped_files += outcome.progress.skipped_files;
        self.progress.failed_files += outcome.progress.failed_files;
        self.progress.logical_bytes_cloned += outcome.progress.logical_bytes_cloned;
        self.progress.bytes_streamed += outcome.progress.bytes_streamed;
        self.progress.cloned_files += outcome.progress.cloned_files;
        self.progress.native_copied_files += outcome.progress.native_copied_files;
        self.progress.buffered_copied_files += outcome.progress.buffered_copied_files;
        self.metrics.transfer_elapsed += outcome.metrics.transfer_elapsed;
        self.metrics.destination_sync_work += outcome.metrics.destination_sync_work;
        self.metrics.source_retirement_elapsed += outcome.metrics.source_retirement_elapsed;
        self.requests += 1;
    }
}

fn sample_engine(config: &Config, prepared: &Prepared) {
    let input_logical_bytes = prepared_size(config);
    apply_budgets(config.workload);
    let requests = prepared
        .transfers
        .iter()
        .map(|transfer| request_for(config, prepared, transfer))
        .collect::<Vec<_>>();
    let before = usage_pair();
    let started = Instant::now();
    let mut outcomes = Vec::with_capacity(requests.len());
    let sink = |_event: Event| Ok::<(), io::Error>(());
    for request in &requests {
        outcomes.push(ish_fs::execute(request, &Cancellation::default(), &sink));
    }
    let elapsed = started.elapsed();
    let after = usage_pair();
    let mut totals = Totals::default();
    for outcome in outcomes {
        totals.add(outcome);
    }
    if config.workload == Workload::Rename {
        assert_eq!(totals.progress.discovered_files, 1, "rename enumerated descendants");
        assert_eq!(totals.progress.completed_files, 1);
        assert_eq!(totals.progress.cloned_files, 0);
        assert_eq!(totals.progress.native_copied_files, 0);
        assert_eq!(totals.progress.buffered_copied_files, 0);
    }
    print_sample(
        config,
        input_logical_bytes,
        SampleTiming {
            elapsed,
            startup_elapsed: None,
            end_to_end: None,
        },
        UsageInterval { before, after },
        Some(&totals),
        None,
    );
}

fn sample_builtin(config: &Config, prepared: &Prepared) {
    let input_logical_bytes = prepared_size(config);
    apply_budgets(config.workload);
    let before = usage_pair();
    let (startup, elapsed, progress_observed, end_to_end) = run_builtin(config, prepared);
    let after = usage_pair();
    print_sample(
        config,
        input_logical_bytes,
        SampleTiming {
            elapsed,
            startup_elapsed: Some(startup),
            end_to_end: Some(end_to_end),
        },
        UsageInterval { before, after },
        None,
        Some(progress_observed),
    );
}

fn sample_native(config: &Config, prepared: &Prepared) {
    let input_logical_bytes = prepared_size(config);
    apply_budgets(config.workload);
    let before = usage_pair();
    let started = Instant::now();
    for transfer in &prepared.transfers {
        run_native_transfer(config, transfer);
    }
    let elapsed = started.elapsed();
    let after = usage_pair();
    print_sample(
        config,
        input_logical_bytes,
        SampleTiming {
            elapsed,
            startup_elapsed: None,
            end_to_end: None,
        },
        UsageInterval { before, after },
        None,
        None,
    );
}

fn run_builtin(config: &Config, prepared: &Prepared) -> (Duration, Duration, bool, Duration) {
    let home = prepared.sample_root.join("home");
    fs::create_dir_all(home.join(".config/ish")).expect("create isolated builtin home");
    let temporary_directory = home.join(".tmp");
    fs::create_dir_all(&temporary_directory).expect("create isolated builtin temp directory");
    let locale = if cfg!(target_os = "linux") {
        TestEnv::hermetic_ascii()
    } else {
        TestEnv::hermetic_utf8("C.UTF-8")
    }
    .expect("create hermetic PTY environment")
    .env("HOME", &home)
    .env("USER", "bench")
    .env("PWD", &config.root)
    .env("TMPDIR", &temporary_directory)
    .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    let command = CommandSpec::new(Path::new(env!("CARGO_BIN_EXE_ish")))
        .current_dir(&config.root)
        .env("HOME", &home)
        .env("USER", "bench")
        .env("PWD", &config.root)
        .env("TMPDIR", &temporary_directory)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    let scenario = Scenario::new("ish fs benchmark")
        .expect("valid benchmark scenario")
        .command(command)
        .environment(locale)
        .size(Size::new(100, 30).expect("nonzero terminal size"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let commands = prepared
        .transfers
        .iter()
        .map(|transfer| {
            let mut command = shell_command(config, transfer).into_bytes();
            command.push(b'\r');
            command
        })
        .collect::<Vec<_>>();
    let total_start = Instant::now();
    let startup_start = total_start;
    let mut terminal = PtyTest::spawn(scenario).expect("spawn interactive ish through PTY");
    let prompt = wait_for_prompt(&mut terminal, "initial prompt");
    let startup = startup_start.elapsed();
    let _ = prompt;
    let mut elapsed = Duration::ZERO;
    for bytes in &commands {
        let command_started = Instant::now();
        terminal
            .send_bytes(terminal.deadline(Duration::from_secs(120)), bytes)
            .expect("send builtin benchmark command");
        terminal
            .wait_for_screen_change(
                terminal.deadline(Duration::from_secs(120)),
                "interactive command echo or completion",
            )
            .expect("observe command input in PTY");
        wait_for_prompt(&mut terminal, "command completion prompt");
        elapsed += command_started.elapsed();
        let status_output_start = terminal.raw_output().len();
        terminal
            .send_bytes(
                terminal.deadline(Duration::from_secs(5)),
                b"echo fs-bench-status-$?\r",
            )
            .expect("send builtin status query");
        terminal
            .wait_for_screen_change(
                terminal.deadline(Duration::from_secs(10)),
                "interactive command status output",
            )
            .expect("observe builtin status output in PTY");
        wait_for_prompt(&mut terminal, "status query prompt");
        assert!(
            terminal.raw_output()[status_output_start..]
                .windows(b"fs-bench-status-0".len())
                .any(|window| window == b"fs-bench-status-0"),
            "interactive builtin returned a nonzero status"
        );
    }
    let progress_observed = terminal
        .raw_output()
        .windows(b" files, ".len())
        .any(|window| window == b" files, ");
    if config.workload == Workload::ProgressOff {
        assert!(!progress_observed, "--no-progress emitted a terminal progress line");
    }
    if config.workload == Workload::ProgressOn {
        assert!(progress_observed, "progress-enabled copy emitted no terminal progress line");
    }
    terminal
        .send_bytes(terminal.deadline(Duration::from_secs(5)), &[0x04])
        .expect("send EOF to ish");
    let status = terminal
        .wait_for_exit(terminal.deadline(Duration::from_secs(10)))
        .expect("ish exits after benchmark commands");
    assert_eq!(status, ptytest::ExitStatus::Code(0), "interactive ish exited unsuccessfully");
    let end_to_end = total_start.elapsed();
    (startup, elapsed, progress_observed, end_to_end)
}

fn wait_for_prompt(terminal: &mut PtyTest, description: &str) -> ptytest::ScreenSnapshot {
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(120)),
            description,
            |screen| {
                let cursor = screen.cursor();
                screen
                    .row(usize::from(cursor.row))
                    .is_some_and(|row| {
                        row.get(..usize::from(cursor.column))
                            .is_some_and(|prefix| prefix.ends_with("$ "))
                    })
            },
        )
        .expect("wait for interactive prompt")
}

fn shell_command(config: &Config, transfer: &Transfer) -> String {
    let command = if config.workload.is_move() { "mv" } else { "cp" };
    let mut parts = vec![command.to_owned()];
    match config.side {
        Side::Builtin => {
            parts.extend(["--jobs".to_owned(), jobs_for(config.workload).to_string()]);
            if transfer.merge {
                parts.push("--merge".to_owned());
            }
            if transfer.cross_file_systems {
                parts.push("--cross-file-systems".to_owned());
            }
            if config.workload.is_move() {
                parts.push("--durable".to_owned());
            }
            if let Some(progress) = config.workload.terminal_progress() {
                if !progress {
                    parts.push("--no-progress".to_owned());
                }
            } else {
                parts.push("--no-progress".to_owned());
            }
        }
        _ => unreachable!("shell command only used for builtin side"),
    }
    parts.push(shell_quote(transfer.source.as_os_str()));
    parts.push(shell_quote(transfer.destination.as_os_str()));
    parts.join(" ")
}

fn shell_quote(path: &OsStr) -> String {
    let value = String::from_utf8(path.as_bytes().to_vec()).expect("benchmark fixture paths are UTF-8");
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_native_transfer(config: &Config, transfer: &Transfer) {
    let program = config.side.native_program().expect("native utility side");
    let mut command = Command::new(program);
    match config.side {
        Side::NativeMove => {
            command.arg(&transfer.source).arg(&transfer.destination);
        }
        Side::NativeCopy | Side::NativeClone => {
            native_cp_arguments(&mut command, config, transfer);
        }
        Side::Engine | Side::Builtin => unreachable!("native utility side"),
    }
    let status = command.status().expect("start native comparison utility");
    assert!(status.success(), "native comparison utility failed: {status}");
}

fn native_cp_arguments(command: &mut Command, config: &Config, transfer: &Transfer) {
    let clone_mode = env::var("ISH_FS_BENCH_CLONE_MODE").unwrap_or_else(|_| "none".to_owned());
    if config.side == Side::NativeClone {
        match clone_mode.as_str() {
            "gnu" => {
                command.arg("--reflink=auto");
                if config.workload == Workload::Sparse {
                    command.arg("--sparse=always");
                }
            }
            "apple" => {
                command.arg("-c");
            }
            _ => panic!("clone-aware cp side requires gnu or apple clone mode"),
        }
    }
    command.arg("-R");
    let preserves_permissions =
        (config.side == Side::NativeClone && clone_mode == "gnu") || cfg!(target_os = "macos");
    if preserves_permissions {
        command.arg("-p");
    }
    if transfer.merge {
        command.arg(transfer.source.join(".")).arg(&transfer.destination);
    } else {
        command.arg(&transfer.source).arg(&transfer.destination);
    }
}

#[derive(Clone, Copy)]
struct SampleTiming {
    elapsed: Duration,
    startup_elapsed: Option<Duration>,
    end_to_end: Option<Duration>,
}

struct UsageInterval {
    before: UsagePair,
    after: UsagePair,
}

#[derive(Clone, Copy)]
struct Usage {
    user_micros: u64,
    system_micros: u64,
    max_rss_bytes: u64,
}

#[derive(Clone, Copy)]
struct UsagePair {
    self_usage: Usage,
    child_usage: Usage,
    nofile_soft: Option<u64>,
    as_soft: Option<u64>,
}

fn usage_pair() -> UsagePair {
    UsagePair {
        self_usage: read_usage(libc::RUSAGE_SELF),
        child_usage: read_usage(libc::RUSAGE_CHILDREN),
        nofile_soft: getrlimit(Resource::Nofile).current,
        as_soft: getrlimit(Resource::As).current,
    }
}

fn read_usage(who: libc::c_int) -> Usage {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` initializes a valid rusage pointer for either supported selector.
    let result = unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
    assert_eq!(result, 0, "read process resource usage");
    // SAFETY: successful `getrusage` initialized the full rusage structure.
    let usage = unsafe { usage.assume_init() };
    Usage {
        user_micros: timeval_micros(usage.ru_utime),
        system_micros: timeval_micros(usage.ru_stime),
        max_rss_bytes: rss_bytes(usage.ru_maxrss),
    }
}

fn timeval_micros(value: libc::timeval) -> u64 {
    (value.tv_sec.max(0) as u64) * 1_000_000 + value.tv_usec.max(0) as u64
}

fn rss_bytes(value: libc::c_long) -> u64 {
    let value = value.max(0) as u64;
    if cfg!(target_os = "macos") {
        value
    } else {
        value.saturating_mul(1024)
    }
}

fn apply_budgets(workload: Workload) {
    if !workload.constrained() {
        return;
    }
    let nofile = getrlimit(Resource::Nofile);
    let nofile_soft = nofile.current.unwrap_or(256).min(64);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(nofile_soft),
            maximum: nofile.maximum,
        },
    )
    .expect("apply constrained descriptor limit");
    let address_space = getrlimit(Resource::As);
    let address_space_soft = address_space.current.unwrap_or(1024 * 1024 * 1024).min(1024 * 1024 * 1024);
    setrlimit(
        Resource::As,
        Rlimit {
            current: Some(address_space_soft),
            maximum: address_space.maximum,
        },
    )
    .expect("apply constrained address-space limit");
}

fn print_sample(
    config: &Config,
    input_logical_bytes: u64,
    timing: SampleTiming,
    usage: UsageInterval,
    totals: Option<&Totals>,
    progress_observed: Option<bool>,
) {
    let SampleTiming {
        elapsed,
        startup_elapsed,
        end_to_end,
    } = timing;
    let UsageInterval { before, after } = usage;
    let binary = Path::new(env!("CARGO_BIN_EXE_ish"));
    let binary_bytes = fs::metadata(binary)
        .expect("stat ish binary for benchmark report")
        .len();
    let self_user = after
        .self_usage
        .user_micros
        .saturating_sub(before.self_usage.user_micros);
    let self_system = after
        .self_usage
        .system_micros
        .saturating_sub(before.self_usage.system_micros);
    let child_user = after
        .child_usage
        .user_micros
        .saturating_sub(before.child_usage.user_micros);
    let child_system = after
        .child_usage
        .system_micros
        .saturating_sub(before.child_usage.system_micros);
    let max_rss_bytes = after
        .self_usage
        .max_rss_bytes
        .max(after.child_usage.max_rss_bytes);
    let (progress, metrics) = totals
        .map(|totals| (Some(totals.progress), Some(totals.metrics)))
        .unwrap_or((None, None));
    let methods = progress.map_or_else(
        || match config.side {
            Side::Builtin => "not_exposed_by_interactive_adapter".to_owned(),
            Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
                "not_exposed_by_native_utility".to_owned()
            }
            Side::Engine => unreachable!("engine samples provide method counts"),
        },
        |progress| {
            format!(
                "clone:{}:native:{}:buffered:{}",
                progress.cloned_files,
                progress.native_copied_files,
                progress.buffered_copied_files
            )
        },
    );
    let files = progress.map_or_else(
        || match config.side {
            Side::Builtin => "not_exposed_by_interactive_adapter".to_owned(),
            Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
                "not_exposed_by_native_utility".to_owned()
            }
            Side::Engine => unreachable!("engine samples provide progress counts"),
        },
        |progress| {
            format!(
                "discovered:{}:completed:{}:skipped:{}:failed:{}",
                progress.discovered_files,
                progress.completed_files,
                progress.skipped_files,
                progress.failed_files
            )
        },
    );
    let bytes = progress.map_or_else(
        || match config.side {
            Side::Builtin => "not_exposed_by_interactive_adapter".to_owned(),
            Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
                "not_exposed_by_native_utility".to_owned()
            }
            Side::Engine => unreachable!("engine samples provide byte counts"),
        },
        |progress| {
            format!(
                "logical_cloned:{}:streamed:{}",
                progress.logical_bytes_cloned, progress.bytes_streamed
            )
        },
    );
    let phase_metrics = metrics.map_or_else(
        || match config.side {
            Side::Builtin => "not_exposed_by_interactive_adapter".to_owned(),
            Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
                "not_exposed_by_native_utility".to_owned()
            }
            Side::Engine => unreachable!("engine samples provide phase metrics"),
        },
        |metrics| {
            format!(
                "transfer_elapsed:{:.9}:destination_sync_work:{:.9}:source_retirement_elapsed:{:.9}",
                metrics.transfer_elapsed.as_secs_f64(),
                metrics.destination_sync_work.as_secs_f64(),
                metrics.source_retirement_elapsed.as_secs_f64()
            )
        },
    );
    let progress_display = progress_observed.map_or_else(
        || "not-applicable".to_owned(),
        |observed| if observed { "observed" } else { "not-observed" }.to_owned(),
    );
    let available_resources = getrlimit(Resource::Nofile);
    let request_count = totals.map_or_else(
        || match config.side {
            Side::Builtin => "not_engine_request_count".to_owned(),
            Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
                "not_engine_request_count".to_owned()
            }
            Side::Engine => unreachable!("engine samples report request count"),
        },
        |totals| totals.requests.to_string(),
    );
    let (elapsed_scope, cpu_scope) = match config.side {
        Side::Engine => ("engine-request-invocations", "measured-operation"),
        Side::Builtin => ("interactive-command-responses", "whole-interactive-session"),
        Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
            ("native-utility-invocations", "measured-operation-and-child-utility")
        }
    };
    let utility_label = env::var("ISH_FS_BENCH_UTILITY_LABEL").unwrap_or_else(|_| "none".to_owned());
    let move_durability = if !config.workload.is_move() {
        "not_applicable_copy"
    } else {
        match config.side {
            Side::Engine => "engine_durable_requested",
            Side::Builtin => "builtin_durable_requested",
            Side::NativeMove => "unverified",
            Side::NativeCopy | Side::NativeClone => unreachable!("copy side for move workload"),
        }
    };
    let requested_jobs = if matches!(
        config.side,
        Side::NativeCopy | Side::NativeClone | Side::NativeMove
    ) {
        "not_applicable".to_owned()
    } else {
        jobs_for(config.workload).to_string()
    };
    let is_native_utility = matches!(
        config.side,
        Side::NativeCopy | Side::NativeClone | Side::NativeMove
    );
    let transfer_buffer_bytes = if is_native_utility {
        "not_applicable_external_utility".to_owned()
    } else {
        TRANSFER_BUFFER_BYTES.to_string()
    };
    let work_queue_capacity = if is_native_utility {
        "not_applicable_external_utility".to_owned()
    } else {
        QUEUE_CAPACITY.to_string()
    };
    let event_delivery = match config.side {
        Side::Engine => "inline_no_queue",
        Side::Builtin => "not_exposed_by_interactive_adapter",
        Side::NativeCopy | Side::NativeClone | Side::NativeMove => {
            "not_applicable_external_utility"
        }
    };
    let record_memory_budget_bytes = if is_native_utility {
        "not_applicable_external_utility".to_owned()
    } else {
        RECORD_MEMORY_BYTES.to_string()
    };
    let descriptor_high_water = if is_native_utility {
        "not_exposed_by_native_utility"
    } else {
        "not_observed"
    };
    let (max_descriptor_budget, reserved_descriptor_budget, min_descriptor_budget) =
        if is_native_utility {
            (
                "not_applicable_external_utility".to_owned(),
                "not_applicable_external_utility".to_owned(),
                "not_applicable_external_utility".to_owned(),
            )
        } else {
            (
                MAX_DESCRIPTORS.to_string(),
                RESERVED_DESCRIPTORS.to_string(),
                MIN_DESCRIPTORS.to_string(),
            )
        };
    let nofile_soft_before = limit_text(before.nofile_soft);
    let nofile_soft_after = limit_text(available_resources.current);
    let address_space_soft_after = limit_text(after.as_soft);
    let startup_elapsed = startup_elapsed.map_or_else(
        || "not_applicable".to_owned(),
        |value| format!("{:.9}", value.as_secs_f64()),
    );
    let end_to_end_elapsed = end_to_end.map_or_else(
        || "not_applicable".to_owned(),
        |value| format!("{:.9}", value.as_secs_f64()),
    );
    println!(
        "fs_bench_sample workload={} side={} sample={} native_utility_label={} move_durability={} cache_state=warm-after-fixture-setup input_logical_bytes={} elapsed_scope={} elapsed_seconds={:.9} startup_elapsed_seconds={} end_to_end_elapsed_seconds={} cpu_scope={} user_cpu_seconds={:.6} system_cpu_seconds={:.6} peak_rss_bytes={} ish_binary_bytes={} requested_jobs={} ish_transfer_buffer_bytes={} ish_work_queue_capacity={} ish_event_delivery={} ish_record_memory_budget_bytes={} ish_descriptor_high_water={} ish_max_descriptor_budget={} ish_reserved_descriptor_budget={} ish_min_descriptor_budget={} nofile_soft_limit_before={} nofile_soft_limit_after={} address_space_soft_limit_after={} requests={} method_counts={} file_counts={} byte_counts={} metrics={} terminal_progress={} physical_io=unknown",
        config.workload.name(),
        config.side.name(),
        config.sample,
        utility_label,
        move_durability,
        input_logical_bytes,
        elapsed_scope,
        elapsed.as_secs_f64(),
        startup_elapsed,
        end_to_end_elapsed,
        cpu_scope,
        (self_user + child_user) as f64 / 1_000_000.0,
        (self_system + child_system) as f64 / 1_000_000.0,
        max_rss_bytes,
        binary_bytes,
        requested_jobs,
        transfer_buffer_bytes,
        work_queue_capacity,
        event_delivery,
        record_memory_budget_bytes,
        descriptor_high_water,
        max_descriptor_budget,
        reserved_descriptor_budget,
        min_descriptor_budget,
        nofile_soft_before,
        nofile_soft_after,
        address_space_soft_after,
        request_count,
        methods,
        files,
        bytes,
        phase_metrics,
        progress_display,
    );
}

fn limit_text(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_owned(), |value| value.to_string())
}

fn verify_merged_tree(source: &Path, seed: &Path, destination: &Path) {
    let mut expected = BTreeMap::new();
    collect_files(source, Path::new(""), &mut expected);
    collect_files(seed, Path::new(""), &mut expected);
    let actual = tree_entries(destination);
    assert_eq!(actual, expected, "merged destination differs from expected files");
}

fn tree_entries(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut entries = BTreeMap::new();
    collect_files(root, Path::new(""), &mut entries);
    entries
}

fn collect_files(root: &Path, relative: &Path, entries: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(root.join(relative)).expect("read compared tree") {
        let entry = entry.expect("read compared tree entry");
        let path = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(root.join(&path)).expect("stat compared tree entry");
        if metadata.is_dir() {
            collect_files(root, &path, entries);
        } else if metadata.file_type().is_symlink() {
            let value = fs::read_link(root.join(&path))
                .unwrap()
                .into_os_string()
                .into_vec();
            entries.insert(path, value);
        } else {
            let value = fs::read(root.join(&path)).expect("read compared file");
            entries.insert(path, value);
        }
    }
}

fn compare_tree(expected: &Path, actual: &Path) {
    let expected_metadata = fs::symlink_metadata(expected).expect("stat expected output");
    let actual_metadata = fs::symlink_metadata(actual).expect("stat actual output");
    assert_eq!(expected_metadata.file_type().is_dir(), actual_metadata.file_type().is_dir());
    assert_eq!(expected_metadata.file_type().is_symlink(), actual_metadata.file_type().is_symlink());
    if expected_metadata.is_dir() {
        let expected_entries = names(expected);
        let actual_entries = names(actual);
        assert_eq!(actual_entries, expected_entries, "directory entries differ");
        for name in expected_entries {
            compare_tree(&expected.join(&name), &actual.join(name));
        }
    } else if expected_metadata.file_type().is_symlink() {
        assert_eq!(fs::read_link(expected).unwrap(), fs::read_link(actual).unwrap());
    } else {
        assert_eq!(fs::read(expected).expect("read expected file"), fs::read(actual).expect("read actual file"));
    }
}

fn names(path: &Path) -> Vec<OsString> {
    let mut names = fs::read_dir(path)
        .expect("read output directory")
        .map(|entry| entry.expect("read output entry").file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn verify_sparse_copy(expected: &Path, actual: &Path, require_sparse: bool) {
    let expected_metadata = fs::metadata(expected).expect("stat expected sparse source");
    let actual_metadata = fs::metadata(actual).expect("stat sparse output");
    assert_eq!(actual_metadata.len(), expected_metadata.len());
    if require_sparse {
        assert!(
            actual_metadata.blocks().saturating_mul(512) < MAX_SPARSE_ALLOCATED_BYTES,
            "sparse output allocated too much space"
        );
    }
    let mut expected_file = fs::File::open(expected).expect("open expected sparse source");
    let mut actual_file = fs::File::open(actual).expect("open sparse output");
    let mut expected_bytes = [0_u8; TRANSFER_BUFFER_BYTES];
    let mut actual_bytes = [0_u8; TRANSFER_BUFFER_BYTES];
    let mut offset = 0_u64;
    while offset < expected_metadata.len() {
        let count = (expected_metadata.len() - offset).min(expected_bytes.len() as u64) as usize;
        expected_file
            .read_exact(&mut expected_bytes[..count])
            .expect("read expected sparse bytes");
        actual_file
            .read_exact(&mut actual_bytes[..count])
            .expect("read copied sparse bytes");
        assert_eq!(
            &actual_bytes[..count],
            &expected_bytes[..count],
            "sparse output differs at byte {offset}"
        );
        offset += count as u64;
    }
}

fn prepared_size(config: &Config) -> u64 {
    let paths: Vec<PathBuf> = match config.workload {
        Workload::TinyCopy | Workload::TinyMove => (0..MOVE_ITERATIONS)
            .map(|index| {
                config
                    .root
                    .join("tiny/source")
                    .join(format!("item-{index:03}"))
            })
            .collect(),
        Workload::Flat => vec![config.root.join("flat/source")],
        Workload::Nested => vec![config.root.join("nested/source")],
        Workload::Deep => vec![config.root.join("deep/source")],
        Workload::Wide | Workload::ProgressOn | Workload::ProgressOff => {
            vec![config.root.join("wide/source")]
        }
        Workload::Large | Workload::Clone => vec![config.root.join("large/source/payload")],
        Workload::Sparse => vec![config.root.join("sparse/source/payload")],
        Workload::Hardlinks => vec![config.root.join("hardlinks/source")],
        Workload::Rename => vec![config.root.join("same-fs-move/source")],
        Workload::Cross => vec![config.root.join("cross-source/source/payload")],
        Workload::Merge => vec![config.root.join("merge/source")],
    };
    paths.iter().map(|path| logical_size(path)).sum()
}

fn logical_size(path: &Path) -> u64 {
    let metadata = fs::symlink_metadata(path).expect("stat benchmark input size");
    if metadata.file_type().is_symlink() {
        0
    } else if metadata.is_dir() {
        fs::read_dir(path)
            .expect("read benchmark input size directory")
            .map(|entry| logical_size(&entry.expect("read benchmark input size entry").path()))
            .sum()
    } else {
        metadata.len()
    }
}

