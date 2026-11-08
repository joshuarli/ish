use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use fdu_scan::{ScanEvent, ScanExclusions};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunMode {
    Automatic,
    Summary,
    Interactive,
}

pub fn run(
    args: &[OsString],
    pwd: PathBuf,
    pending_input: Vec<u8>,
) -> (i32, Vec<u8>) {
    let mut apparent = false;
    let mut read_only = false;
    let mut mode = RunMode::Automatic;
    let mut exclude_paths = Vec::new();
    let mut target_path = None;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_os_str();
        let bytes = arg.as_bytes();
        index += 1;

        if !positional_only {
            if bytes == b"--" {
                positional_only = true;
                continue;
            }
            if bytes == b"-h" || bytes == b"--help" {
                let _ = print_help();
                return (0, pending_input);
            }
            if bytes == b"--rmrf" {
                let _ = writeln!(
                    io::stderr(),
                    "fdu: --rmrf is not supported; ish provides native rmrf. Unlike fdu --rmrf, which empties a directory while preserving it, rmrf PATH permanently removes the named directory itself."
                );
                return (2, pending_input);
            }
            if bytes == b"-a" || bytes == b"--apparent" {
                apparent = true;
                continue;
            }
            if bytes == b"--read-only" {
                read_only = true;
                continue;
            }
            if bytes == b"--summary" {
                mode = RunMode::Summary;
                continue;
            }
            if bytes == b"--interactive" {
                mode = RunMode::Interactive;
                continue;
            }
            if bytes == b"-x" || bytes == b"--exclude" {
                let Some(val) = args.get(index) else {
                    let _ = writeln!(io::stderr(), "ish: fdu: option '{}' requires an argument", arg.to_string_lossy());
                    let _ = writeln!(io::stderr(), "{}", usage());
                    return (2, pending_input);
                };
                exclude_paths.push(PathBuf::from(val));
                index += 1;
                continue;
            }
            if bytes.starts_with(b"-x") && bytes.len() > 2 {
                let path_bytes = &bytes[2..];
                exclude_paths.push(PathBuf::from(OsStr::from_bytes(path_bytes)));
                continue;
            }
            if bytes.starts_with(b"-") && bytes.len() > 1 {
                let _ = writeln!(io::stderr(), "ish: fdu: unrecognized option '{}'", arg.to_string_lossy());
                let _ = writeln!(io::stderr(), "{}", usage());
                return (2, pending_input);
            }
        }

        if target_path.is_some() {
            let _ = writeln!(io::stderr(), "ish: fdu: unexpected argument '{}'", arg.to_string_lossy());
            let _ = writeln!(io::stderr(), "{}", usage());
            return (2, pending_input);
        }
        target_path = Some(PathBuf::from(arg));
    }

    let exclusions = match ScanExclusions::from_paths(exclude_paths) {
        Ok(excl) => excl,
        Err(error) => {
            let _ = writeln!(io::stderr(), "ish: fdu: {error}");
            return (2, pending_input);
        }
    };

    let resolved_path = match target_path {
        Some(p) => p,
        None => pwd,
    };

    let terminal_available = io::stdin().is_terminal() && io::stdout().is_terminal();
    let is_interactive = match mode {
        RunMode::Summary => false,
        RunMode::Interactive if !terminal_available => {
            let _ = writeln!(io::stderr(), "ish: fdu: --interactive requires terminal input and output");
            return (1, pending_input);
        }
        RunMode::Interactive => true,
        RunMode::Automatic => terminal_available,
    };

    if is_interactive {
        crate::fdu_browser::run(resolved_path, read_only, apparent, exclusions, pending_input)
    } else {
        match run_summary(&resolved_path, apparent, exclusions) {
            Ok(()) => (0, pending_input),
            Err(error) => {
                let _ = writeln!(io::stderr(), "ish: fdu: {error}");
                (1, pending_input)
            }
        }
    }
}

fn print_help() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Usage: fdu [OPTIONS] [PATH]\n")?;
    writeln!(stdout, "Browse disk usage in a terminal or write a summary of directory sizes.\n")?;
    writeln!(stdout, "Options:")?;
    writeln!(stdout, "  -a, --apparent           show logical file lengths rather than filesystem usage")?;
    writeln!(stdout, "  -x, --exclude PATH       omit an exact path relative to PATH (can be repeated)")?;
    writeln!(stdout, "      --summary            print directory sizes and exit without starting the browser")?;
    writeln!(stdout, "      --interactive        require terminal input and output to open the browser")?;
    writeln!(stdout, "      --read-only          disable permanent deletion in the browser")?;
    writeln!(stdout, "  -h, --help               display this help and exit")?;
    stdout.flush()
}

#[allow(dead_code)]
pub(crate) fn help() -> String {
    "Usage: fdu [--apparent] [-x PATH | --exclude PATH]... [--summary | --interactive] [--read-only] [PATH]\nBrowse disk usage in a terminal or write a summary of directory sizes.\n".to_owned()
}

fn usage() -> &'static str {
    "Usage: fdu [--apparent] [-x PATH | --exclude PATH]... [--summary | --interactive] [--read-only] [PATH]"
}

fn run_summary(path: &Path, apparent: bool, exclusions: ScanExclusions) -> io::Result<()> {
    let anchor = fdu_scan::open_root(path)?;
    let mut tree = fdu_core::Tree::new(&anchor.name, anchor.identity, anchor.link_count)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root index name too long"))?;
    assert!(tree.set_modified_time(tree.root(), anchor.modified_time()));
    let mut tokens = vec![Some(tree.root())];
    let (event_sender, events) = std::sync::mpsc::sync_channel(512);
    let (returned_batch_sender, returned_batches) = std::sync::mpsc::sync_channel(8);
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = fdu_scan::start_indexed_scan_with_exclusions(
        anchor,
        exclusions,
        event_sender,
        returned_batches,
        std::sync::Arc::clone(&cancelled),
    );

    let mut skipped_entries = 0usize;
    let mut mount_boundaries = 0usize;
    let mut unsupported_aliases = 0usize;

    loop {
        let event = match events.recv() {
            Ok(e) => e,
            Err(_) => break,
        };
        match event {
            ScanEvent::Entries(batch) => {
                let outcome = fdu_core::fold_entry_batch(&mut tree, &mut tokens, &batch);
                skipped_entries = skipped_entries.saturating_add(outcome.incomplete_entries);
                let _ = returned_batch_sender.send(batch);
            }
            ScanEvent::DirectoryFinished { directory, complete } => {
                fdu_core::finish_scan_directory(&mut tree, &tokens, directory, complete);
            }
            ScanEvent::DirectoriesFinished(batch) => {
                for (directory, complete) in batch {
                    fdu_core::finish_scan_directory(&mut tree, &tokens, directory, complete);
                }
            }
            ScanEvent::DirectoryExcluded { directory, reason } => {
                if let Some(node) = tokens.get(directory.0 as usize).copied().flatten() {
                    tree.set_state(node, fdu_core::NodeStatus::Excluded(reason));
                }
                match reason {
                    fdu_core::ExclusionReason::MountBoundary => {
                        mount_boundaries = mount_boundaries.saturating_add(1);
                    }
                    fdu_core::ExclusionReason::UnsupportedAlias => {
                        unsupported_aliases = unsupported_aliases.saturating_add(1);
                    }
                }
            }
            ScanEvent::DirectoryFailed { .. } | ScanEvent::Failed { .. } => {
                skipped_entries = skipped_entries.saturating_add(1);
            }
            ScanEvent::DirectoryWaiting { .. } => {}
            ScanEvent::Started { .. } | ScanEvent::RootListed | ScanEvent::Cancelled | ScanEvent::Finished => {}
        }
    }
    let _ = worker.join();

    if skipped_entries != 0 {
        eprintln!(
            "fdu: warning: skipped {skipped_entries} entries after errors; size totals may be incomplete"
        );
    }
    if mount_boundaries != 0 {
        eprintln!(
            "fdu: warning: excluded {mount_boundaries} filesystem or mount boundaries; totals omit their contents"
        );
    }
    if unsupported_aliases != 0 {
        eprintln!(
            "fdu: warning: skipped {unsupported_aliases} unsupported directory aliases; affected totals omit their contents"
        );
    }

    let mut child_dirs = Vec::new();
    for child in tree.children(tree.root()) {
        let record = tree.record(child).expect("child record exists");
        if record.entry_type == fdu_core::EntryType::Directory {
            let name = tree.name(child).unwrap_or_default().to_vec();
            let size = if apparent {
                record.apparent_bytes
            } else {
                record.allocated_bytes
            };
            let exclusion = match record.state {
                fdu_core::NodeState::Excluded(fdu_core::ExclusionReason::MountBoundary) => {
                    Some("MOUNT BOUNDARY")
                }
                fdu_core::NodeState::Excluded(fdu_core::ExclusionReason::UnsupportedAlias) => {
                    Some("UNSUPPORTED DIRECTORY ALIAS")
                }
                _ => None,
            };
            child_dirs.push((name, size, exclusion));
        }
    }

    child_dirs.sort_unstable_by(|a, b| {
        a.2.is_some()
            .cmp(&b.2.is_some())
            .then_with(|| b.1.cmp(&a.1))
    });

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for (name_bytes, size, exclusion) in child_dirs {
        let name_str = std::ffi::OsStr::from_bytes(&name_bytes);
        if let Some(excl) = exclusion {
            writeln!(output, "{excl}\t{name_str:?}")?;
        } else {
            writeln!(output, "{}\t{name_str:?}", format_summary_size(size))?;
        }
    }
    output.flush()?;
    Ok(())
}

fn format_summary_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }

    let bytes = u128::from(bytes);
    let mut divisor = 1024u128;
    let mut unit = 1usize;
    let tenths = loop {
        let rounded = (bytes * 10 + divisor / 2) / divisor;
        if rounded < 10_240 || unit == UNITS.len() - 1 {
            break rounded;
        }
        divisor *= 1024;
        unit += 1;
    };
    let value = if tenths % 10 == 0 {
        (tenths / 10).to_string()
    } else {
        format!("{}.{:01}", tenths / 10, tenths % 10)
    };
    format!("{value}{}", UNITS[unit])
}
