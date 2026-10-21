use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MARKER: &str = ".ish-fs-bench-owner";
const SMALL_BYTES: u64 = 4096;
const LARGE_BYTES: u64 = 128 * 1024 * 1024;
const SPARSE_BYTES: u64 = 1024 * 1024 * 1024;
const CROSS_FLAT_FILES: u64 = 16_384;
const DRY_RUN_BRANCHES: u64 = 64;
const DRY_RUN_BRANCH_FILES: u64 = 64;
const DRY_RUN_DEPTH: u64 = 16;
const MAX_SPARSE_ALLOCATED_BYTES: u64 = 16 * 1024 * 1024;
const PATTERN_CHUNK: usize = 64 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("fs_bench: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut arguments = env::args_os();
    let _program = arguments.next();
    let command = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage)?;
    match command.as_str() {
        "prepare" => {
            let root = required_path(&mut arguments)?;
            let cross_parent = arguments.next().map(PathBuf::from);
            reject_extra(arguments)?;
            prepare(&root, cross_parent.as_deref())
        }
        "verify" => {
            let root = required_path(&mut arguments)?;
            let identity = required_identity(&mut arguments)?;
            let cross = arguments.next().map(PathBuf::from);
            let cross_identity = arguments.next().map(parse_identity).transpose()?;
            reject_extra(arguments)?;
            verify(&root, identity, cross.as_deref(), cross_identity)
        }
        "cleanup" => {
            let root = required_path(&mut arguments)?;
            let identity = required_identity(&mut arguments)?;
            let cross = arguments.next().map(PathBuf::from);
            let cross_identity = arguments.next().map(parse_identity).transpose()?;
            reject_extra(arguments)?;
            cleanup(&root, identity, cross.as_deref(), cross_identity)
        }
        _ => Err(usage()),
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: fs_bench prepare ROOT [CROSS_FILESYSTEM_PARENT] | verify ROOT DEVICE:INODE [CROSS_ROOT CROSS_DEVICE:INODE] | cleanup ROOT DEVICE:INODE [CROSS_ROOT CROSS_DEVICE:INODE]",
    )
}

fn required_path(arguments: &mut impl Iterator<Item = std::ffi::OsString>) -> io::Result<PathBuf> {
    arguments.next().map(PathBuf::from).ok_or_else(usage)
}

fn required_identity(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> io::Result<(u64, u64)> {
    let value = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage)?;
    parse_identity(value.into())
}

fn parse_identity(value: std::ffi::OsString) -> io::Result<(u64, u64)> {
    let value = value.into_string().map_err(|_| usage())?;
    let (device, inode) = value
        .split_once(':')
        .ok_or_else(usage)?;
    let device = device.parse().map_err(|_| usage())?;
    let inode = inode.parse().map_err(|_| usage())?;
    Ok((device, inode))
}

fn reject_extra(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> io::Result<()> {
    if arguments.next().is_some() {
        return Err(usage());
    }
    Ok(())
}

fn temp_root() -> io::Result<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::canonicalize(manifest.join("../../.work/tmp"))
}

fn validate_owned_location(path: &Path, parent_must_exist: bool) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture roots must use absolute paths",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture root has no parent"))?;
    let expected = temp_root()?;
    let canonical_parent = if parent_must_exist {
        fs::canonicalize(parent)?
    } else {
        parent.to_path_buf()
    };
    if canonical_parent != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("fixture root must be a direct child of {}", expected.display()),
        ));
    }
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture root has no name"))?;
    Ok(expected.join(name))
}

fn prepare(root: &Path, cross_parent: Option<&Path>) -> io::Result<()> {
    let root = validate_owned_location(root, true)?;
    fs::create_dir(&root)?;
    let identity = root_identity(&root)?;
    let mut cross_fixture = None;
    let preparation: io::Result<()> = (|| {
        write_marker(&root, identity)?;
        create_fixtures(&root)?;
        verify_fixtures(&root)?;
        cross_fixture = create_cross_fixture(&root, identity, cross_parent)?;
        if cross_fixture.is_some() {
            create_local_cross_source(&root)?;
            verify_local_cross_source(&root)?;
        }
        Ok(())
    })();
    if let Err(preparation_error) = preparation {
        let cleanup_cross_error = cross_fixture
            .as_ref()
            .and_then(|(cross, cross_identity)| cleanup_cross(cross, *cross_identity, identity).err());
        let cleanup_root_error = remove_created_directory(&root, identity).err();
        if let Some(cleanup_error) = cleanup_cross_error.or(cleanup_root_error) {
            return Err(io::Error::other(format!(
                "fixture preparation failed: {preparation_error}; cleanup failed: {cleanup_error}"
            )));
        }
        return Err(preparation_error);
    }
    println!("root={}", root.display());
    println!("root_identity={}:{}", identity.0, identity.1);
    print_fixture_summary(&root)?;
    Ok(())
}

fn root_identity(root: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "fixture root is not a real directory"));
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn remove_created_directory(path: &Path, expected: (u64, u64)) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() || (metadata.dev(), metadata.ino()) != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to remove a fixture directory whose identity changed",
        ));
    }
    fs::remove_dir_all(path)
}

fn write_marker(root: &Path, identity: (u64, u64)) -> io::Result<()> {
    let contents = format!("format=1\ndevice={}\ninode={}\n", identity.0, identity.1);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(MARKER))?
        .write_all(contents.as_bytes())
}

fn verify_owner(root: &Path, expected: (u64, u64)) -> io::Result<()> {
    let root = validate_owned_location(root, true)?;
    let actual = root_identity(&root)?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("fixture root identity changed: expected {expected:?}, found {actual:?}"),
        ));
    }
    let marker = fs::read_to_string(root.join(MARKER))?;
    if !marker.lines().any(|line| line == format!("device={}", expected.0))
        || !marker.lines().any(|line| line == format!("inode={}", expected.1))
        || !marker.lines().any(|line| line == "format=1")
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture ownership marker does not match"));
    }
    Ok(())
}

fn verify(
    root: &Path,
    identity: (u64, u64),
    cross: Option<&Path>,
    cross_identity: Option<(u64, u64)>,
) -> io::Result<()> {
    let root = validate_owned_location(root, true)?;
    verify_owner(&root, identity)?;
    verify_fixtures(&root)?;
    match (cross, cross_identity) {
        (Some(cross), Some(cross_identity)) => {
            verify_cross_owner(cross, cross_identity, identity)?;
            verify_local_cross_source(&root)?;
        }
        (None, None) => {}
        _ => return Err(usage()),
    }
    print_fixture_summary(&root)
}

fn cleanup(
    root: &Path,
    identity: (u64, u64),
    cross: Option<&Path>,
    cross_identity: Option<(u64, u64)>,
) -> io::Result<()> {
    let root = validate_owned_location(root, true)?;
    verify_owner(&root, identity)?;
    match (cross, cross_identity) {
        (Some(cross), Some(cross_identity)) => cleanup_cross(cross, cross_identity, identity)?,
        (None, None) => {}
        _ => return Err(usage()),
    }
    fs::remove_dir_all(&root)
}

fn create_fixtures(root: &Path) -> io::Result<()> {
    let tiny = root.join("tiny");
    fs::create_dir_all(tiny.join("source"))?;
    fs::create_dir_all(tiny.join("destination"))?;
    for index in 0..96_u64 {
        write_pattern(
            &tiny.join("source").join(format!("item-{index:03}")),
            512,
            index,
        )?;
    }

    let flat = root.join("flat");
    fs::create_dir_all(flat.join("source"))?;
    fs::create_dir_all(flat.join("destination"))?;
    for index in 0..4096_u64 {
        write_pattern(
            &flat.join("source").join(format!("item-{index:04}")),
            SMALL_BYTES,
            index,
        )?;
    }

    let nested = root.join("nested");
    let mut directory = nested.join("source");
    fs::create_dir_all(&directory)?;
    for depth in 0..8_u64 {
        for index in 0..32_u64 {
            write_pattern(
                &directory.join(format!("item-{index:03}")),
                256 * 1024,
                depth * 1000 + index,
            )?;
        }
        directory = directory.join(format!("level-{depth:02}"));
        fs::create_dir(&directory)?;
    }
    fs::create_dir_all(nested.join("destination"))?;

    let deep = root.join("deep").join("source");
    fs::create_dir_all(&deep)?;
    directory = deep.clone();
    for depth in 0..96_u64 {
        write_pattern(&directory.join("leaf"), 1024, depth)?;
        directory = directory.join(format!("d{depth:02}"));
        fs::create_dir(&directory)?;
    }
    fs::create_dir_all(root.join("deep").join("destination"))?;

    let wide = root.join("wide");
    fs::create_dir_all(wide.join("source"))?;
    fs::create_dir_all(wide.join("destination"))?;
    for branch in 0..64_u64 {
        let branch_root = wide.join("source").join(format!("branch-{branch:02}"));
        let destination_branch = wide.join("destination").join(format!("branch-{branch:02}"));
        fs::create_dir(&branch_root)?;
        fs::create_dir(&destination_branch)?;
        for index in 0..64_u64 {
            write_pattern(
                &branch_root.join(format!("item-{index:02}")),
                SMALL_BYTES,
                branch * 1000 + index,
            )?;
            write_pattern(
                &destination_branch.join(format!("item-{index:02}")),
                SMALL_BYTES,
                branch * 1000 + index + 1_000_000,
            )?;
        }
    }

    let dry_run = root.join("dry-run");
    fs::create_dir_all(dry_run.join("source"))?;
    fs::create_dir_all(dry_run.join("destination"))?;
    for branch in 0..DRY_RUN_BRANCHES {
        let branch_name = format!("branch-{branch:02}");
        let source_branch = dry_run.join("source").join(&branch_name);
        let destination_branch = dry_run.join("destination").join(&branch_name);
        fs::create_dir(&source_branch)?;
        fs::create_dir(&destination_branch)?;
        for index in 0..DRY_RUN_BRANCH_FILES {
            let name = format!("item-{index:02}");
            write_pattern(&source_branch.join(&name), SMALL_BYTES, branch * 1000 + index)?;
            write_pattern(&destination_branch.join(&name), SMALL_BYTES, branch * 1000 + index + 1_000_000)?;
        }
        write_pattern(&destination_branch.join("destination-only"), SMALL_BYTES, branch + 2_000_000)?;
        let mut source = source_branch.join("deep");
        let mut destination = destination_branch.join("deep");
        fs::create_dir(&source)?;
        fs::create_dir(&destination)?;
        for depth in 0..DRY_RUN_DEPTH {
            let seed = branch * 1000 + depth + 100_000;
            write_pattern(&source.join("leaf"), SMALL_BYTES, seed)?;
            write_pattern(&destination.join("leaf"), SMALL_BYTES, seed + 1_000_000)?;
            if depth + 1 < DRY_RUN_DEPTH {
                let directory = format!("d{depth:02}");
                source.push(&directory);
                destination.push(directory);
                fs::create_dir(&source)?;
                fs::create_dir(&destination)?;
            }
        }
    }

    let merge = root.join("merge");
    fs::create_dir_all(merge.join("source"))?;
    fs::create_dir_all(merge.join("destination"))?;
    for branch in 0..8_u64 {
        let source_branch = merge.join("source").join(format!("branch-{branch:02}"));
        let destination_branch = merge.join("destination").join(format!("branch-{branch:02}"));
        fs::create_dir(&source_branch)?;
        fs::create_dir(&destination_branch)?;
        for index in 0..64_u64 {
            write_pattern(
                &source_branch.join(format!("new-{index:03}")),
                64 * 1024,
                branch * 1000 + index,
            )?;
            write_pattern(
                &destination_branch.join(format!("existing-{index:03}")),
                4096,
                branch * 1000 + index + 500,
            )?;
        }
    }

    let large = root.join("large");
    fs::create_dir_all(large.join("source"))?;
    fs::create_dir_all(large.join("destination"))?;
    write_pattern(&large.join("source").join("payload"), LARGE_BYTES, 777)?;

    let sparse = root.join("sparse");
    fs::create_dir_all(sparse.join("source"))?;
    fs::create_dir_all(sparse.join("destination"))?;
    create_sparse(&sparse.join("source").join("payload"))?;

    let hardlinks = root.join("hardlinks");
    fs::create_dir_all(hardlinks.join("source"))?;
    fs::create_dir_all(hardlinks.join("destination"))?;
    write_pattern(&hardlinks.join("source").join("first"), SMALL_BYTES, 8801)?;
    for index in 0..16_u64 {
        fs::hard_link(
            hardlinks.join("source").join("first"),
            hardlinks.join("source").join(format!("group-{index:02}")),
        )?;
    }
    fs::hard_link(hardlinks.join("source").join("first"), hardlinks.join("source").join("alias"))?;
    for index in 0..256_u64 {
        write_pattern(&hardlinks.join("source").join(format!("ordinary-{index:03}")), SMALL_BYTES, 9900 + index)?;
    }

    let same_fs = root.join("same-fs-move");
    fs::create_dir_all(same_fs.join("destination-parent"))?;
    fs::create_dir(same_fs.join("source"))?;
    fs::create_dir(same_fs.join("source").join("nested"))?;
    write_pattern(&same_fs.join("source").join("nested").join("payload"), 8 * 1024 * 1024, 8810)?;

    Ok(())
}

fn create_cross_fixture(
    root: &Path,
    owner_identity: (u64, u64),
    parent: Option<&Path>,
) -> io::Result<Option<(PathBuf, (u64, u64))>> {
    let Some(parent) = parent else {
        println!("cross_filesystem=unavailable:not_designated");
        return Ok(None);
    };
    let parent = fs::canonicalize(parent)?;
    let expected = temp_root()?;
    if !parent.starts_with(&expected) || parent == expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cross-filesystem parent must be a designated mount below {}", expected.display()),
        ));
    }
    let root_metadata = fs::metadata(root)?;
    let parent_metadata = fs::metadata(&parent)?;
    if root_metadata.dev() == parent_metadata.dev() {
        println!("cross_filesystem=unavailable:same_device");
        return Ok(None);
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let cross = parent.join(format!("ish-fs-bench-cross-{}-{unique}", std::process::id()));
    fs::create_dir(&cross)?;
    let identity = root_identity(&cross)?;
    if let Err(error) = write_cross_marker(&cross, identity, owner_identity) {
        return match remove_created_directory(&cross, identity) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(io::Error::other(format!(
                "cross-filesystem fixture preparation failed: {error}; cleanup failed: {cleanup_error}"
            ))),
        };
    }
    println!("cross_filesystem=available");
    println!("cross_root={}", cross.display());
    println!("cross_identity={}:{}", identity.0, identity.1);
    Ok(Some((cross, identity)))
}

fn validate_cross_location(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cross-filesystem roots must use absolute paths"));
    }
    let path = fs::canonicalize(path)?;
    let expected = temp_root()?;
    if !path.starts_with(&expected) || path == expected {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "cross-filesystem root must be below .work/tmp"));
    }
    Ok(path)
}

fn write_cross_marker(root: &Path, identity: (u64, u64), owner: (u64, u64)) -> io::Result<()> {
    let contents = format!(
        "format=1\ndevice={}\ninode={}\nowner_device={}\nowner_inode={}\n",
        identity.0, identity.1, owner.0, owner.1
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(MARKER))?
        .write_all(contents.as_bytes())
}

fn verify_cross_owner(root: &Path, expected: (u64, u64), owner: (u64, u64)) -> io::Result<()> {
    let root = validate_cross_location(root)?;
    let actual = root_identity(&root)?;
    if actual != expected {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "cross-filesystem fixture identity changed"));
    }
    let marker = fs::read_to_string(root.join(MARKER))?;
    if !marker.lines().any(|line| line == format!("device={}", expected.0))
        || !marker.lines().any(|line| line == format!("inode={}", expected.1))
        || !marker.lines().any(|line| line == "format=1")
        || !marker.lines().any(|line| line == format!("owner_device={}", owner.0))
        || !marker.lines().any(|line| line == format!("owner_inode={}", owner.1))
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "cross-filesystem ownership marker does not match"));
    }
    Ok(())
}

fn cleanup_cross(root: &Path, identity: (u64, u64), owner: (u64, u64)) -> io::Result<()> {
    let root = validate_cross_location(root)?;
    verify_cross_owner(&root, identity, owner)?;
    fs::remove_dir_all(root)
}

fn create_local_cross_source(root: &Path) -> io::Result<()> {
    let source = root.join("cross-source/source");
    fs::create_dir_all(&source)?;
    write_pattern(&source.join("payload"), 64 * 1024 * 1024, 9901)?;
    let flat = source.join("flat");
    fs::create_dir(&flat)?;
    for index in 0..CROSS_FLAT_FILES {
        write_pattern(&flat.join(format!("item-{index:05}")), SMALL_BYTES, 10_000 + index)?;
    }
    Ok(())
}

fn verify_local_cross_source(root: &Path) -> io::Result<()> {
    verify_pattern(
        &root.join("cross-source/source/payload"),
        64 * 1024 * 1024,
        9901,
    )?;
    for index in 0..CROSS_FLAT_FILES {
        verify_pattern(&root.join("cross-source/source/flat").join(format!("item-{index:05}")), SMALL_BYTES, 10_000 + index)?;
    }
    Ok(())
}

fn write_pattern(path: &Path, length: u64, seed: u64) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let mut bytes = vec![0_u8; PATTERN_CHUNK];
    let mut offset = 0_u64;
    while offset < length {
        let count = (length - offset).min(bytes.len() as u64) as usize;
        fill_pattern(&mut bytes[..count], offset, seed);
        file.write_all(&bytes[..count])?;
        offset += count as u64;
    }
    Ok(())
}

fn verify_pattern(path: &Path, length: u64, seed: u64) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != length {
        return Err(io::Error::other(format!("{} has length {}, expected {length}", path.display(), metadata.len())));
    }
    let mut file = File::open(path)?;
    let mut actual = vec![0_u8; PATTERN_CHUNK];
    let mut expected = vec![0_u8; PATTERN_CHUNK];
    let mut offset = 0_u64;
    while offset < length {
        let count = (length - offset).min(actual.len() as u64) as usize;
        file.read_exact(&mut actual[..count])?;
        fill_pattern(&mut expected[..count], offset, seed);
        if actual[..count] != expected[..count] {
            return Err(io::Error::other(format!("{} differs at byte {offset}", path.display())));
        }
        offset += count as u64;
    }
    Ok(())
}

fn fill_pattern(bytes: &mut [u8], offset: u64, seed: u64) {
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = ((offset + index as u64 + seed * 131) % 251) as u8;
    }
}

fn create_sparse(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_len(SPARSE_BYTES)?;
    for (offset, seed) in [0_u64, SPARSE_BYTES / 2, SPARSE_BYTES - 1024].into_iter().zip([101_u64, 202, 303]) {
        file.seek(SeekFrom::Start(offset))?;
        let mut extent = vec![0_u8; 1024];
        fill_pattern(&mut extent, offset, seed);
        file.write_all(&extent)?;
    }
    file.sync_all()
}

fn verify_sparse(path: &Path) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != SPARSE_BYTES {
        return Err(io::Error::other(format!("sparse payload has length {}, expected {SPARSE_BYTES}", metadata.len())));
    }
    let allocated = metadata.blocks().saturating_mul(512);
    if allocated >= MAX_SPARSE_ALLOCATED_BYTES {
        return Err(io::Error::new(io::ErrorKind::Unsupported, format!("sparse fixture is dense: {allocated} bytes allocated for {} logical bytes", metadata.len())));
    }
    let mut file = File::open(path)?;
    let mut actual = vec![0_u8; 1024];
    for (offset, seed) in [0_u64, SPARSE_BYTES / 2, SPARSE_BYTES - 1024].into_iter().zip([101_u64, 202, 303]) {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut actual)?;
        let mut expected = vec![0_u8; actual.len()];
        fill_pattern(&mut expected, offset, seed);
        if actual != expected {
            return Err(io::Error::other(format!("sparse extent differs at byte {offset}")));
        }
    }
    file.seek(SeekFrom::Start(SPARSE_BYTES / 4))?;
    file.read_exact(&mut actual)?;
    if actual.iter().any(|byte| *byte != 0) {
        return Err(io::Error::other("sparse hole contains nonzero bytes"));
    }
    Ok(())
}

fn verify_fixtures(root: &Path) -> io::Result<()> {
    for index in 0..96_u64 {
        verify_pattern(&root.join("tiny/source").join(format!("item-{index:03}")), 512, index)?;
    }
    for index in 0..4096_u64 {
        verify_pattern(&root.join("flat/source").join(format!("item-{index:04}")), SMALL_BYTES, index)?;
    }
    for depth in 0..8_u64 {
        let directory = (0..depth).fold(root.join("nested/source"), |path, parent| path.join(format!("level-{parent:02}")));
        for index in 0..32_u64 {
            verify_pattern(&directory.join(format!("item-{index:03}")), 256 * 1024, depth * 1000 + index)?;
        }
    }
    let mut directory = root.join("deep/source");
    for depth in 0..96_u64 {
        verify_pattern(&directory.join("leaf"), 1024, depth)?;
        directory = directory.join(format!("d{depth:02}"));
    }
    for branch in 0..64_u64 {
        let directory = root.join("wide/source").join(format!("branch-{branch:02}"));
        let destination = root.join("wide/destination").join(format!("branch-{branch:02}"));
        for index in 0..64_u64 {
            verify_pattern(&directory.join(format!("item-{index:02}")), SMALL_BYTES, branch * 1000 + index)?;
            verify_pattern(&destination.join(format!("item-{index:02}")), SMALL_BYTES, branch * 1000 + index + 1_000_000)?;
        }
    }
    for branch in 0..DRY_RUN_BRANCHES {
        let branch_name = format!("branch-{branch:02}");
        let source_branch = root.join("dry-run/source").join(&branch_name);
        let destination_branch = root.join("dry-run/destination").join(&branch_name);
        for index in 0..DRY_RUN_BRANCH_FILES {
            let name = format!("item-{index:02}");
            verify_pattern(&source_branch.join(&name), SMALL_BYTES, branch * 1000 + index)?;
            verify_pattern(&destination_branch.join(&name), SMALL_BYTES, branch * 1000 + index + 1_000_000)?;
        }
        verify_pattern(&destination_branch.join("destination-only"), SMALL_BYTES, branch + 2_000_000)?;
        let mut source = source_branch.join("deep");
        let mut destination = destination_branch.join("deep");
        for depth in 0..DRY_RUN_DEPTH {
            let seed = branch * 1000 + depth + 100_000;
            verify_pattern(&source.join("leaf"), SMALL_BYTES, seed)?;
            verify_pattern(&destination.join("leaf"), SMALL_BYTES, seed + 1_000_000)?;
            if depth + 1 < DRY_RUN_DEPTH {
                let directory = format!("d{depth:02}");
                source.push(&directory);
                destination.push(directory);
            }
        }
    }
    for branch in 0..8_u64 {
        let source_branch = root.join("merge/source").join(format!("branch-{branch:02}"));
        let destination_branch = root.join("merge/destination").join(format!("branch-{branch:02}"));
        for index in 0..64_u64 {
            verify_pattern(&source_branch.join(format!("new-{index:03}")), 64 * 1024, branch * 1000 + index)?;
            verify_pattern(&destination_branch.join(format!("existing-{index:03}")), 4096, branch * 1000 + index + 500)?;
        }
    }
    verify_pattern(&root.join("large/source/payload"), LARGE_BYTES, 777)?;
    verify_sparse(&root.join("sparse/source/payload"))?;
    verify_pattern(&root.join("hardlinks/source/first"), SMALL_BYTES, 8801)?;
    for index in 0..256_u64 {
        verify_pattern(&root.join("hardlinks/source").join(format!("ordinary-{index:03}")), SMALL_BYTES, 9900 + index)?;
    }
    let first = fs::metadata(root.join("hardlinks/source/first"))?;
    let alias = fs::metadata(root.join("hardlinks/source/alias"))?;
    let last = fs::metadata(root.join("hardlinks/source/group-15"))?;
    if first.dev() != alias.dev() || first.ino() != alias.ino() || first.dev() != last.dev() || first.ino() != last.ino() || first.nlink() < 18 {
        return Err(io::Error::other("hardlink fixture names do not identify one inode"));
    }
    verify_pattern(&root.join("same-fs-move/source/nested/payload"), 8 * 1024 * 1024, 8810)?;
    let source_device = fs::metadata(root.join("same-fs-move/source"))?.dev();
    let destination_device = fs::metadata(root.join("same-fs-move/destination-parent"))?.dev();
    if source_device != destination_device {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "same-filesystem move fixture is on different devices"));
    }
    Ok(())
}

fn print_fixture_summary(root: &Path) -> io::Result<()> {
    let rows = [
        ("tiny", 96_u64, 96 * 512),
        ("flat", 4096, 4096 * SMALL_BYTES),
        ("nested", 256, 8 * 32 * 256 * 1024),
        ("deep", 96, 96 * 1024),
        ("wide", 4096, 64 * 64 * SMALL_BYTES),
        ("dry-run-wide", DRY_RUN_BRANCHES * (DRY_RUN_BRANCH_FILES + DRY_RUN_DEPTH), DRY_RUN_BRANCHES * (DRY_RUN_BRANCH_FILES + DRY_RUN_DEPTH) * SMALL_BYTES),
        ("merge", 1024, 8 * 64 * (64 * 1024 + 4096)),
        ("large", 1, LARGE_BYTES),
        ("sparse", 1, SPARSE_BYTES),
        ("hardlinks", 274, 274 * SMALL_BYTES),
        ("same-fs-move", 1, 8 * 1024 * 1024),
    ];
    for (name, files, bytes) in rows {
        println!("fixture={name} regular_entries={files} logical_bytes={bytes}");
    }
    if root.join("cross-source/source/flat").exists() {
        println!("fixture=cross-flat regular_entries={CROSS_FLAT_FILES} logical_bytes={}", CROSS_FLAT_FILES * SMALL_BYTES);
    }
    let device = fs::metadata(root)?.dev();
    println!("fixture_device={device}");
    let cross_bytes = if root.join("cross-source/source/payload").exists() {
        64 * 1024 * 1024 + CROSS_FLAT_FILES * SMALL_BYTES
    } else {
        0
    };
    println!(
        "fixture_bytes_written={}",
        96 * 512
            + 16 * 1024 * 1024
            + 64 * 1024 * 1024
            + 96 * 1024
            + 16 * 1024 * 1024
            + 16 * 1024 * 1024
            + DRY_RUN_BRANCHES * (2 * DRY_RUN_BRANCH_FILES + 2 * DRY_RUN_DEPTH + 1) * SMALL_BYTES
            + 8 * 64 * (64 * 1024 + 4096)
            + LARGE_BYTES
            + 3 * 1024
            + 257 * SMALL_BYTES
            + 8 * 1024 * 1024
            + cross_bytes
    );
    Ok(())
}
