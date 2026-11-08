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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FduOutcome {
    Completed { status: i32, unconsumed: Vec<u8> },
    Terminated { signal: i32 },
}

pub fn run(
    args: &[OsString],
    pwd: PathBuf,
    pending_input: Vec<u8>,
    ls_colors: Option<&OsStr>,
) -> FduOutcome {
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
                return FduOutcome::Completed { status: 0, unconsumed: pending_input };
            }
            if bytes == b"--rmrf" {
                let _ = writeln!(
                    io::stderr(),
                    "fdu: --rmrf is not supported; ish provides native rmrf. Unlike fdu --rmrf, which empties a directory while preserving it, rmrf PATH permanently removes the named directory itself."
                );
                return FduOutcome::Completed { status: 2, unconsumed: pending_input };
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
                if mode == RunMode::Interactive {
                    let _ = writeln!(io::stderr(), "ish: fdu: --summary and --interactive cannot be used together");
                    let _ = writeln!(io::stderr(), "{}", usage());
                    return FduOutcome::Completed { status: 2, unconsumed: pending_input };
                }
                mode = RunMode::Summary;
                continue;
            }
            if bytes == b"--interactive" {
                if mode == RunMode::Summary {
                    let _ = writeln!(io::stderr(), "ish: fdu: --summary and --interactive cannot be used together");
                    let _ = writeln!(io::stderr(), "{}", usage());
                    return FduOutcome::Completed { status: 2, unconsumed: pending_input };
                }
                mode = RunMode::Interactive;
                continue;
            }
            if bytes == b"-x" || bytes == b"--exclude" {
                let Some(val) = args.get(index) else {
                    let _ = writeln!(io::stderr(), "ish: fdu: option '{}' requires an argument", arg.to_string_lossy());
                    let _ = writeln!(io::stderr(), "{}", usage());
                    return FduOutcome::Completed { status: 2, unconsumed: pending_input };
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
                return FduOutcome::Completed { status: 2, unconsumed: pending_input };
            }
        }

        if target_path.is_some() {
            let _ = writeln!(io::stderr(), "ish: fdu: unexpected argument '{}'", arg.to_string_lossy());
            let _ = writeln!(io::stderr(), "{}", usage());
            return FduOutcome::Completed { status: 2, unconsumed: pending_input };
        }
        target_path = Some(PathBuf::from(arg));
    }

    let exclusions = match ScanExclusions::from_paths(exclude_paths) {
        Ok(excl) => excl,
        Err(error) => {
            let _ = writeln!(io::stderr(), "ish: fdu: {error}");
            return FduOutcome::Completed { status: 2, unconsumed: pending_input };
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
            return FduOutcome::Completed { status: 1, unconsumed: pending_input };
        }
        RunMode::Interactive => true,
        RunMode::Automatic => terminal_available,
    };

    if is_interactive {
        crate::fdu_browser::run(resolved_path, read_only, apparent, exclusions, pending_input, ls_colors)
    } else {
        let summary_res = run_summary(&resolved_path, apparent, exclusions);
        if let Some(sig) = crate::signal::take_pending_termination() {
            return FduOutcome::Terminated { signal: sig };
        }
        match summary_res {
            Ok(()) => FduOutcome::Completed { status: 0, unconsumed: pending_input },
            Err(error) => {
                let _ = writeln!(io::stderr(), "ish: fdu: {error}");
                FduOutcome::Completed { status: 1, unconsumed: pending_input }
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

struct SummaryChild {
    token: Option<fdu_scan::DirectoryToken>,
    name: Vec<u8>,
    apparent_bytes: u64,
    allocated_bytes: u64,
    exclusion: Option<fdu_core::ExclusionReason>,
}

fn run_summary(path: &Path, apparent: bool, exclusions: ScanExclusions) -> io::Result<()> {
    let anchor = fdu_scan::open_root(path)?;
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
    let mut scan_failed = false;
    let mut failure_message = None;

    let mut top_dirs: Vec<SummaryChild> = Vec::new();
    let mut token_to_top: Vec<Option<u32>> = Vec::new();

    while let Ok(event) = events.recv() {
        if let Some(sig) = crate::signal::take_pending_termination() {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(returned_batch_sender);
            let _ = worker.join();
            crate::signal::note_termination(sig);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan interrupted"));
        }
        match event {
            ScanEvent::Entries(batch) => {
                for entry in &batch.entries {
                    let parent_token = entry.parent.0 as usize;
                    if parent_token == 0 {
                        if entry.entry_type == fdu_core::EntryType::Directory {
                            let name = batch
                                .names
                                .get(entry.name.start as usize..entry.name.end as usize)
                                .unwrap_or_default()
                                .to_vec();
                            let exclusion = match entry.state {
                                fdu_core::NodeStatus::Excluded(r) => Some(r),
                                _ => None,
                            };
                            let idx = top_dirs.len() as u32;
                            top_dirs.push(SummaryChild {
                                token: entry.directory_token,
                                name,
                                apparent_bytes: entry.apparent_bytes,
                                allocated_bytes: entry.allocated_bytes,
                                exclusion,
                            });
                            if let Some(token) = entry.directory_token {
                                let t_idx = token.0 as usize;
                                if token_to_top.len() <= t_idx {
                                    token_to_top.resize(t_idx + 1, None);
                                }
                                token_to_top[t_idx] = Some(idx);
                            }
                        }
                    } else if let Some(Some(top_idx)) = token_to_top.get(parent_token) {
                        let top_idx = *top_idx as usize;
                        if top_idx < top_dirs.len() {
                            top_dirs[top_idx].apparent_bytes = top_dirs[top_idx]
                                .apparent_bytes
                                .saturating_add(entry.apparent_bytes);
                            top_dirs[top_idx].allocated_bytes = top_dirs[top_idx]
                                .allocated_bytes
                                .saturating_add(entry.allocated_bytes);
                        }
                        if entry.entry_type == fdu_core::EntryType::Directory
                            && let Some(token) = entry.directory_token
                        {
                            let t_idx = token.0 as usize;
                            if token_to_top.len() <= t_idx {
                                token_to_top.resize(t_idx + 1, None);
                            }
                            token_to_top[t_idx] = Some(top_idx as u32);
                        }
                    }
                }
                let _ = returned_batch_sender.try_send(batch);
            }
            ScanEvent::DirectoryFinished { .. } | ScanEvent::DirectoriesFinished(_) => {}
            ScanEvent::DirectoryExcluded { directory, reason } => {
                let d_idx = directory.0 as usize;
                if let Some(Some(top_idx)) = token_to_top.get(d_idx) {
                    let top_idx = *top_idx as usize;
                    if top_idx < top_dirs.len() && top_dirs[top_idx].token == Some(directory) {
                        top_dirs[top_idx].exclusion = Some(reason);
                    }
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
            ScanEvent::DirectoryFailed { .. } => {
                skipped_entries = skipped_entries.saturating_add(1);
            }
            ScanEvent::Failed { message } => {
                scan_failed = true;
                failure_message = Some(message);
            }
            ScanEvent::DirectoryWaiting { .. } => {}
            ScanEvent::Started { .. } | ScanEvent::RootListed | ScanEvent::Cancelled | ScanEvent::Finished => {}
        }
    }
    let join_result = worker.join();
    if join_result.is_err() {
        return Err(io::Error::other("scanner worker terminated unexpectedly"));
    }
    if scan_failed {
        return Err(io::Error::other(failure_message.unwrap_or_else(|| "scan failed".to_owned())));
    }

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
    for child in top_dirs {
        let size = if apparent {
            child.apparent_bytes
        } else {
            child.allocated_bytes
        };
        let exclusion = match child.exclusion {
            Some(fdu_core::ExclusionReason::MountBoundary) => {
                Some("MOUNT BOUNDARY")
            }
            Some(fdu_core::ExclusionReason::UnsupportedAlias) => {
                Some("UNSUPPORTED DIRECTORY ALIAS")
            }
            _ => None,
        };
        child_dirs.push((child.name, size, exclusion));
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

#[cfg(test)]
mod tests {
    use fdu_core::{DirectoryToken, EntryBatch, EntryType, FileIdentity, ModifiedTime, NodeStatus, ScanEntry, ScanEvent, Tree};

    #[test]
    fn fast_producer_more_than_eight_batches_does_not_deadlock_on_recycle() {
        let (event_sender, events) = std::sync::mpsc::sync_channel(512);
        let (returned_batch_sender, returned_batches) = std::sync::mpsc::sync_channel(8);

        let producer = std::thread::spawn(move || {
            for i in 0..12 {
                let mut batch = EntryBatch::with_capacity(4, 64);
                batch.entries.push(ScanEntry {
                    parent: DirectoryToken(0),
                    name: 0..4,
                    directory_token: None,
                    entry_type: EntryType::RegularFile,
                    identity: FileIdentity { device: 1, inode: (i + 1) as u64 },
                    modified_time: ModifiedTime::from_unix_seconds(0).unwrap(),
                    link_count: 1,
                    apparent_bytes: 100,
                    allocated_bytes: 100,
                    state: NodeStatus::Complete,
                });
                batch.names.extend_from_slice(b"file");
                if event_sender.send(ScanEvent::Entries(batch)).is_err() {
                    break;
                }
            }
            let _ = event_sender.send(ScanEvent::Finished);
        });

        producer.join().unwrap();
        drop(returned_batches);

        let mut tree = Tree::new(b"root", FileIdentity { device: 1, inode: 0 }, 1).unwrap();
        let mut tokens = vec![Some(tree.root())];

        let mut batches_processed = 0;
        while let Ok(event) = events.recv() {
            if let ScanEvent::Entries(batch) = event {
                let _ = fdu_core::fold_entry_batch(&mut tree, &mut tokens, &batch);
                let _ = returned_batch_sender.try_send(batch);
                batches_processed += 1;
            }
        }
        assert_eq!(batches_processed, 12);
    }
}
