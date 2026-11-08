use ish_fs::sync::{SyncOperands, SyncOptions, SyncRequest};
use ish_fs::{Context, Jobs};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use epsh::error::{ExitStatus, ShellError};

struct Invocation {
    request: SyncRequest,
    verbose: bool,
    no_progress: bool,
}

enum ParsedCommand {
    Help,
    Run(Invocation),
}

#[derive(Debug)]
struct SyntaxError(String);

pub fn run(
    arguments: &[OsString],
    context: Context,
) -> Result<ExitStatus, ShellError> {
    let invocation = match parse(arguments, context) {
        Ok(ParsedCommand::Help) => {
            return match io::stdout().write_all(help().as_bytes()) {
                Ok(()) => Ok(ExitStatus::SUCCESS),
                Err(_) => Ok(ExitStatus::FAILURE),
            };
        }
        Ok(ParsedCommand::Run(invocation)) => invocation,
        Err(error) => {
            let message = format!("ish: rsync: {}\n{}\n", error.0, usage());
            return Ok(if io::stderr().write_all(message.as_bytes()).is_ok() {
                ExitStatus::MISUSE
            } else {
                ExitStatus::FAILURE
            });
        }
    };

    let mut foreground = match crate::foreground::Foreground::start() {
        Ok(foreground) => foreground,
        Err(error) => {
            let _ = writeln!(io::stderr(), "ish: rsync: {error}");
            return Ok(ExitStatus::FAILURE);
        }
    };
    let cancellation = foreground.cancellation();
    let verbose = invocation.verbose;
    let no_progress = invocation.no_progress;
    let show_delete = invocation.request.options.delete;
    let request = invocation.request;
    let result = foreground.run(
        move |cancellation, events| {
            let sink = move |event: ish_fs::Event| events.emit(event);
            ish_fs::sync::execute(&request, &cancellation, &sink)
        },
        |foreground, event| {
            match event {
                ish_fs::Event::Progress(progress) if !no_progress => {
                    foreground.report_progress_line(&format!(
                        "rsync: {} examined, {} transferred, {} skipped, {} bytes",
                        progress.discovered_files,
                        progress.completed_files,
                        progress.skipped_files,
                        progress.bytes_streamed
                    ))
                }
                ish_fs::Event::Progress(_) => foreground.clear_progress(),
                ish_fs::Event::Diagnostic(diagnostic) => {
                    let _ = foreground.clear_progress();
                    writeln!(
                        io::stderr(),
                        "ish: rsync: {}: {}",
                        diagnostic.path.display(),
                        diagnostic.message
                    )
                }
                _ => Ok(()),
            }
        },
    );
    let finish = foreground.finish();

    if cancellation.reason() == Some(ish_fs::CancelReason::User) {
        let _ = writeln!(io::stderr(), "ish: rsync: cancelled");
        return Err(ShellError::Cancelled);
    }
    let outcome = match (result, finish) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(error), Err(finish_error)) => {
            let _ = writeln!(
                io::stderr(),
                "ish: rsync: {error}; foreground cleanup failed: {finish_error}"
            );
            return Ok(ExitStatus::FAILURE);
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => {
            let _ = writeln!(io::stderr(), "ish: rsync: {error}");
            return Ok(ExitStatus::FAILURE);
        }
    };

    if verbose {
        if show_delete && outcome.progress.deleted_files > 0 {
            let _ = writeln!(
                io::stderr(),
                "ish: rsync: {} examined, {} transferred, {} skipped, {} deleted, {} bytes",
                outcome.progress.examined_files,
                outcome.progress.transferred_files,
                outcome.progress.skipped_files,
                outcome.progress.deleted_files,
                outcome.progress.bytes_transferred
            );
        } else {
            let _ = writeln!(
                io::stderr(),
                "ish: rsync: {} examined, {} transferred, {} skipped, {} bytes",
                outcome.progress.examined_files,
                outcome.progress.transferred_files,
                outcome.progress.skipped_files,
                outcome.progress.bytes_transferred
            );
        }
    }

    Ok(ExitStatus::from(outcome.status()))
}

fn parse(
    arguments: &[OsString],
    context: Context,
) -> Result<ParsedCommand, SyntaxError> {
    let mut options = SyncOptions::default();
    let mut verbose = false;
    let mut no_progress = false;
    let mut contents = false;
    let mut into = None;
    let mut operands = Vec::new();
    let mut options_enabled = true;
    let mut jobs_seen = false;
    let mut help = false;
    let mut index = 0;

    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        index += 1;
        if options_enabled && argument == OsStr::new("--") {
            options_enabled = false;
            continue;
        }
        if options_enabled && (argument == OsStr::new("-h") || argument == OsStr::new("--help")) {
            help = true;
            continue;
        }
        if !options_enabled || !is_option(argument) {
            operands.push(PathBuf::from(argument));
            continue;
        }

        match argument.as_bytes() {
            b"--into" => {
                if into.is_some() {
                    return Err(SyntaxError("--into may be used only once".to_owned()));
                }
                let Some(directory) = arguments.get(index) else {
                    return Err(SyntaxError("--into requires a directory".to_owned()));
                };
                into = Some(PathBuf::from(directory));
                index += 1;
            }
            b"--contents" => contents = true,
            b"--delete" => options.delete = true,
            b"--dry-run" => options.dry_run = true,
            b"-n" | b"--no-clobber" => {
                return Err(SyntaxError("-n is not supported; use --dry-run".to_owned()));
            }
            b"-j" | b"--jobs" => {
                if jobs_seen {
                    return Err(SyntaxError("--jobs may be set only once".to_owned()));
                }
                let Some(value) = arguments.get(index) else {
                    return Err(SyntaxError("--jobs requires a positive integer".to_owned()));
                };
                let count = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or_else(|| SyntaxError("--jobs requires a positive integer".to_owned()))?;
                options.jobs = Some(
                    Jobs::new(count)
                        .map_err(|_| SyntaxError("--jobs must be in 1..=32".to_owned()))?,
                );
                jobs_seen = true;
                index += 1;
            }
            b"--verbose" => verbose = true,
            b"--no-progress" => no_progress = true,
            _ => {
                return Err(SyntaxError(format!(
                    "unrecognized option '{}'",
                    argument.to_string_lossy()
                )));
            }
        }
    }

    if help {
        return Ok(ParsedCommand::Help);
    }

    if into.is_some() && contents {
        return Err(SyntaxError(
            "--into and --contents cannot be combined".to_owned(),
        ));
    }

    let sync_operands = if let Some(directory) = into {
        if operands.is_empty() {
            return Err(SyntaxError("missing source operand".to_owned()));
        }
        SyncOperands::Into {
            directory,
            sources: operands,
        }
    } else if contents {
        if operands.is_empty() {
            return Err(SyntaxError("missing operand".to_owned()));
        }
        if operands.len() != 2 {
            return Err(SyntaxError(
                "--contents requires exactly one source directory and one target directory"
                    .to_owned(),
            ));
        }
        let dir = operands.pop().unwrap();
        let src = operands.pop().unwrap();
        SyncOperands::Contents {
            source: src,
            directory: dir,
        }
    } else {
        if operands.len() < 2 {
            return Err(SyntaxError("missing destination operand".to_owned()));
        }
        if operands.len() == 2 {
            let target = &operands[1];
            let target_path = context.cwd.join(target);
            let is_dir = std::fs::metadata(&target_path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                let dir = operands.pop().unwrap();
                SyncOperands::Into {
                    directory: dir,
                    sources: operands,
                }
            } else {
                let dest = operands.pop().unwrap();
                let src = operands.pop().unwrap();
                SyncOperands::Exact {
                    source: src,
                    destination: dest,
                }
            }
        } else {
            let target = operands.pop().unwrap();
            let target_path = context.cwd.join(&target);
            let is_dir = std::fs::metadata(&target_path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if !is_dir {
                return Err(SyntaxError(format!(
                    "target '{}' is not a directory",
                    target.display()
                )));
            }
            SyncOperands::Into {
                directory: target,
                sources: operands,
            }
        }
    };

    Ok(ParsedCommand::Run(Invocation {
        request: SyncRequest {
            operands: sync_operands,
            options,
            context,
        },
        verbose,
        no_progress,
    }))
}

fn is_option(argument: &OsStr) -> bool {
    let bytes = argument.as_bytes();
    bytes.starts_with(b"-") && bytes.len() > 1
}

pub(crate) fn help() -> String {
    "Usage: rsync [OPTIONS] SOURCE... DIRECTORY\n       rsync [OPTIONS] --into DIRECTORY SOURCE...\n       rsync [OPTIONS] --contents SOURCE DIRECTORY\n       rsync [OPTIONS] SOURCE DEST\n\nSynchronize files and directories locally.\n\nOptions:\n      --into DIRECTORY     synchronize sources into target directory\n      --contents           synchronize directory contents without creating top directory\n      --delete             delete extraneous files from destination\n      --dry-run            perform a trial run with no changes made\n  -j, --jobs N             number of parallel transfer jobs (1..=32)\n      --no-progress        suppress progress output\n      --verbose            increase verbosity\n  -h, --help               display this help and exit\n"
        .to_owned()
}

fn usage() -> &'static str {
    "Usage: rsync [OPTIONS] SOURCE... DIRECTORY\n       rsync [OPTIONS] --into DIRECTORY SOURCE...\n       rsync [OPTIONS] --contents SOURCE DIRECTORY\n       rsync [OPTIONS] SOURCE DEST"
}
