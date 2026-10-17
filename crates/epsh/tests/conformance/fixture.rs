use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

use super::parser::{Case, FileKind};

pub struct CaseScratch {
    directory: TempDir,
    pub cwd: PathBuf,
    pub stdin: PathBuf,
    pub script: Option<PathBuf>,
}

impl CaseScratch {
    pub fn create(case: &Case) -> Result<Self, String> {
        let temp_root = conformance_temp_root()?;
        let directory = tempfile::Builder::new()
            .prefix("epsh-conformance-")
            .tempdir_in(temp_root)
            .map_err(|error| format!("{}: cannot create fixture: {error}", case.name))?;
        let cwd = directory.path().join("cwd");
        DirBuilder::new()
            .mode(0o700)
            .create(&cwd)
            .map_err(|error| format!("{}: cannot create working directory: {error}", case.name))?;

        let stdin = directory.path().join("stdin");
        if let Some(contents) = &case.stdin {
            fs::write(&stdin, contents)
                .map_err(|error| format!("{}: cannot write stdin fixture: {error}", case.name))?;
        }

        let script = case.script.as_ref().map(|_| directory.path().join("script"));
        if let (Some(path), Some(contents)) = (&script, &case.script) {
            fs::write(path, contents)
                .map_err(|error| format!("{}: cannot write script fixture: {error}", case.name))?;
        }

        for file in &case.files {
            create_file_fixture(&cwd, file)
                .map_err(|error| format!("{}: {error}", case.name))?;
        }

        Ok(Self { directory, cwd, stdin, script })
    }

    pub fn close(self) -> Result<(), String> {
        self.directory.close().map_err(|error| format!("cannot remove fixture: {error}"))
    }
}

fn conformance_temp_root() -> Result<PathBuf, String> {
    let manifest = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .map_err(|error| format!("cannot resolve package directory: {error}"))?;
    let repository = manifest.ancestors().find(|path| path.join(".git").exists()).unwrap_or(&manifest);
    let root = repository.join(".work/tmp");
    let mut prefix = PathBuf::new();
    for component in root.components() {
        prefix.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(&prefix) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(format!("cannot create conformance scratch directory: {error}")),
                }
                fs::symlink_metadata(&prefix)
                    .map_err(|error| format!("cannot inspect conformance scratch directory: {error}"))?
            }
            Err(error) => return Err(format!("cannot inspect conformance scratch directory: {error}")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("conformance scratch path contains a symlink or non-directory".to_owned());
        }
    }
    Ok(root)
}

fn create_file_fixture(cwd: &Path, file: &super::parser::FileSetup) -> Result<(), String> {
    let relative = Path::new(OsStr::from_bytes(&file.path));
    validate_fixture_path(relative)?;
    let path = confined_path(cwd, relative)?;
    match file.kind {
        FileKind::File => {
            reject_symlink_leaf(&path)?;
            let mut output = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .map_err(|error| format!("cannot create file fixture: {error}"))?;
            std::io::Write::write_all(&mut output, &file.contents)
                .map_err(|error| format!("cannot write file fixture: {error}"))?;
            fs::set_permissions(&path, Permissions::from_mode(file.mode))
                .map_err(|error| format!("cannot set file fixture permissions: {error}"))?;
        }
        FileKind::Directory => {
            reject_existing_path(&path)?;
            DirBuilder::new()
                .mode(file.mode)
                .create(&path)
                .map_err(|error| format!("cannot create directory fixture: {error}"))?;
        }
        FileKind::Symlink => {
            reject_existing_path(&path)?;
            validate_symlink_target(cwd, relative, &file.contents)?;
            symlink(OsStr::from_bytes(&file.contents), &path)
                .map_err(|error| format!("cannot create symlink fixture: {error}"))?;
        }
    }
    Ok(())
}

fn validate_fixture_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("fixture path is absolute or empty".to_owned());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("fixture path escapes its working directory".to_owned());
            }
        }
    }
    Ok(())
}

fn confined_path(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for component in &components[..components.len().saturating_sub(1)] {
        let Component::Normal(name) = component else { continue };
        path.push(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("fixture parent is unavailable: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("fixture path traverses a symlink or non-directory".to_owned());
        }
    }
    let Some(Component::Normal(leaf)) = components.last() else {
        return Err("fixture path has no final component".to_owned());
    };
    path.push(leaf);
    Ok(path)
}

fn reject_symlink_leaf(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err("fixture would write through a symlink".to_owned())
        }
        Ok(metadata) if !metadata.is_file() => Err("file fixture collides with a non-file".to_owned()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect file fixture: {error}")),
    }
}

fn reject_existing_path(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err("fixture path already exists".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect fixture path: {error}")),
    }
}

fn validate_symlink_target(root: &Path, link: &Path, target: &[u8]) -> Result<(), String> {
    let target_path = Path::new(OsStr::from_bytes(target));
    if target_path.is_absolute() || target.contains(&0) {
        return Err("symlink fixture target is absolute or invalid".to_owned());
    }
    let mut normalized = link.parent().unwrap_or_else(|| Path::new(".")).components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(OsString::from(name)),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target_path.components() {
        match component {
            Component::Normal(name) => normalized.push(OsString::from(name)),
            Component::CurDir => {}
            Component::ParentDir if normalized.pop().is_some() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("symlink fixture target escapes its working directory".to_owned());
            }
        }
    }
    let mut resolved = root.to_path_buf();
    for component in normalized {
        resolved.push(component);
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("symlink fixture target traverses another symlink".to_owned());
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err("symlink fixture target traverses a non-directory".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("cannot inspect symlink target: {error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::{confined_path, validate_fixture_path};

    #[test]
    fn case_scratch_stays_in_work_root_with_ambient_tmpdir() {
        const CHILD: &str = "EPSH_CONFORMANCE_SCRATCH_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let cases = super::super::parser::parse_corpus(b"name: scratch-root\nscript: true\n---\n").unwrap();
            let scratch = super::CaseScratch::create(&cases[0]).expect("repository-owned scratch");
            assert_eq!(std::fs::read(scratch.script.as_ref().unwrap()).unwrap(), b"true\n");
            let root = scratch.cwd.parent().unwrap().parent().unwrap();
            assert_eq!(root.file_name(), Some(std::ffi::OsStr::new("tmp")));
            assert_eq!(root.parent().unwrap().file_name(), Some(std::ffi::OsStr::new(".work")));
            scratch.close().unwrap();
            return;
        }
        for ambient in [None, Some("/var/folders/ambient-test-tmpdir")] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "conformance::fixture::tests::case_scratch_stays_in_work_root_with_ambient_tmpdir"]);
            command.env(CHILD, "1");
            match ambient {
                Some(path) => { command.env("TMPDIR", path); }
                None => { command.env_remove("TMPDIR"); }
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{ambient:?}: {}", String::from_utf8_lossy(&output.stdout));
        }
    }

    #[test]
    fn rejects_parent_components_and_existing_symlink_parents() {
        assert!(validate_fixture_path(std::path::Path::new("../outside")).is_err());
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(confined_path(root.path(), std::path::Path::new("linked/file")).is_err());
    }

    #[test]
    fn accepts_non_utf8_fixture_components_without_conversion() {
        let root = tempfile::tempdir().unwrap();
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff".to_vec()));
        assert!(validate_fixture_path(&path).is_ok());
        assert!(confined_path(root.path(), &path).is_ok());
    }
}
