use ish_fs::{
    Collision, Context, Diagnostic, Event, Jobs, OperandOutcome, OperandState, Operands,
    Operation, Options, Request,
};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use epsh::error::{ExitStatus, ShellError};

struct Invocation {
    request: Request,
    verbose: bool,
    no_progress: bool,
}

enum ParsedCommand {
    Help,
    Run(Invocation),
}

#[derive(Debug)]
struct SyntaxError(String);

pub(crate) fn run(
    operation: Operation,
    arguments: &[OsString],
    context: Context,
) -> Result<ExitStatus, ShellError> {
    let invocation = match parse(operation, arguments, context) {
        Ok(ParsedCommand::Help) => {
            return match io::stdout().write_all(help(operation).as_bytes()) {
                Ok(()) => Ok(ExitStatus::SUCCESS),
                Err(_) => Ok(ExitStatus::FAILURE),
            };
        }
        Ok(ParsedCommand::Run(invocation)) => invocation,
        Err(error) => {
            let message = format!(
                "ish: {}: {}\n{}",
                operation_name(operation),
                error.0,
                usage(operation)
            );
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
            let _ = writeln!(io::stderr(), "ish: {}: {error}", operation_name(operation));
            return Ok(ExitStatus::FAILURE);
        }
    };
    let cancellation = foreground.cancellation();
    let verbose = invocation.verbose;
    let no_progress = invocation.no_progress;
    let dry_run = invocation.request.options.dry_run;
    let operation_name = operation_name(operation);
    let mut completed_transfers = 0;
    let mut retained_operands = 0;
    let result = foreground.run(
        move |cancellation, events| ish_fs::execute(&invocation.request, &cancellation, events),
        |foreground, event| {
            if let Event::Progress(progress) = &event {
                completed_transfers = completed_transfers.max(progress.completed_files);
            }
            if let Event::Operand(outcome) = &event
                && matches!(outcome.state,
                    OperandState::Cancelled | OperandState::FailedBeforePublication
                    | OperandState::DestinationPublishedSourceRetained
                    | OperandState::DestinationPartiallyPublished | OperandState::SourcePartiallyRetired)
            {
                retained_operands += 1;
            }
            render_event(
                foreground,
                operation,
                operation_name,
                verbose,
                no_progress,
                dry_run,
                event,
            )
        },
    );
    let finish = foreground.finish();

    if cancellation.reason() == Some(ish_fs::CancelReason::User) {
        let _ = writeln!(io::stderr(), "ish: {operation_name}: cancelled: {completed_transfers} entries processed; {retained_operands} source operands unfinished");
        return Err(ShellError::Cancelled);
    }
    let outcome = match (result, finish) {
        (Ok(outcome), Ok(())) => outcome,
        (Err(error), Err(finish_error)) => {
            let _ = writeln!(io::stderr(), "ish: {operation_name}: {error}; foreground cleanup failed: {finish_error}");
            return Ok(ExitStatus::FAILURE);
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => {
            let _ = writeln!(io::stderr(), "ish: {operation_name}: {error}");
            return Ok(ExitStatus::FAILURE);
        }
    };

    if outcome.progress.skipped_files > 0
        && writeln!(
            io::stderr(),
            "ish: {operation_name}: skipped {} existing entries",
            outcome.progress.skipped_files
        )
        .is_err()
    {
        cancellation.cancel_for_output();
        return Ok(ExitStatus::FAILURE);
    }

    Ok(ExitStatus::from(outcome.status()))
}

fn parse(
    operation: Operation,
    arguments: &[OsString],
    context: Context,
) -> Result<ParsedCommand, SyntaxError> {
    let mut options = Options::default();
    let mut verbose = false;
    let mut no_progress = false;
    let mut into = None;
    let mut operands = Vec::new();
    let mut options_enabled = true;
    let mut jobs_seen = false;
    let mut replace_seen = false;
    let mut no_clobber_seen = false;
    let mut help = false;
    let mut index = 0;

    while index < arguments.len() {
        let argument = arguments[index].as_os_str();
        index += 1;
        if options_enabled && argument == OsStr::new("--") {
            options_enabled = false;
            continue;
        }
        if options_enabled && argument == OsStr::new("-h")
            || options_enabled && argument == OsStr::new("--help")
        {
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
            b"--merge" => options.merge = true,
            b"--replace" => {
                options.collision = Collision::Replace;
                replace_seen = true;
            }
            b"-n" | b"--no-clobber" => {
                options.collision = Collision::Skip;
                no_clobber_seen = true;
            }
            b"--dry-run" => options.dry_run = true,
            b"--durable" => options.durable = true,
            b"--cross-file-systems" => options.cross_file_systems = true,
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
            b"-v" | b"--verbose" => verbose = true,
            b"--no-progress" => no_progress = true,
            b"-r" | b"-R" if operation == Operation::Copy => {}
            b"-r" | b"-R" => {
                return Err(SyntaxError(format!(
                    "{} does not accept recursive flags",
                    operation_name(operation)
                )));
            }
            _ => {
                return Err(SyntaxError(format!(
                    "unknown option {}",
                    escape_bytes(argument.as_bytes())
                )));
            }
        }
    }

    if replace_seen && no_clobber_seen {
        return Err(SyntaxError(
            "--replace cannot be used with --no-clobber".to_owned(),
        ));
    }
    if help {
        return Ok(ParsedCommand::Help);
    }

    let operands = match into {
        Some(directory) => {
            let mut sources = operands.into_iter();
            let Some(first) = sources.next() else {
                return Err(SyntaxError(
                    "--into requires at least one source operand".to_owned(),
                ));
            };
            Operands::Into {
                directory,
                first,
                rest: sources.collect(),
            }
        }
        None => {
            if operands.len() < 2 {
                return Err(SyntaxError(
                    "expected SOURCE DESTINATION, SOURCE... DIRECTORY, or --into DIRECTORY SOURCE..."
                        .to_owned(),
                ));
            }
            if operands.len() == 2 {
                let mut operands = operands.into_iter();
                Operands::Exact {
                    source: operands.next().unwrap(),
                    destination: operands.next().unwrap(),
                }
            } else {
                let directory = operands.pop().unwrap();
                let mut sources = operands.into_iter();
                Operands::Into {
                    directory,
                    first: sources.next().unwrap(),
                    rest: sources.collect(),
                }
            }
        }
    };

    Ok(ParsedCommand::Run(Invocation {
        request: Request {
            operation,
            operands,
            options,
            context,
        },
        verbose,
        no_progress,
    }))
}

fn is_option(argument: &OsStr) -> bool {
    argument.as_bytes().starts_with(b"-") && argument.as_bytes() != b"-"
}

fn render_event(
    foreground: &mut crate::foreground::Foreground,
    operation: Operation,
    label: &str,
    verbose: bool,
    no_progress: bool,
    dry_run: bool,
    event: Event,
) -> io::Result<()> {
    match event {
        Event::Progress(progress) if !no_progress => foreground.report_progress_line(&format!(
            "{label}: {}/{} files, {} bytes streamed, {} bytes cloned",
            progress.completed_files,
            progress.discovered_files,
            progress.bytes_streamed,
            progress.logical_bytes_cloned
        )),
        Event::Progress(_) => foreground.clear_progress(),
        Event::Diagnostic(diagnostic) => {
            foreground.clear_progress()?;
            render_diagnostic(operation, &diagnostic)
        }
        Event::Operand(outcome) => {
            if verbose || dry_run {
                if let Some(message) = render_operand(operation, &outcome, dry_run) {
                    foreground.clear_progress()?;
                    writeln!(io::stderr(), "ish: {label}: {message}")?;
                }
            } else if matches!(
                outcome.state,
                OperandState::DestinationPublishedSourceRetained
                    | OperandState::DestinationPartiallyPublished
                    | OperandState::SourcePartiallyRetired
                    | OperandState::SourceRemovedSyncFailed
                    | OperandState::Cancelled
            ) {
                foreground.clear_progress()?;
                writeln!(io::stderr(), "ish: {label}: {}", render_partial(&outcome))?;
            }
            Ok(())
        }
    }
}

fn render_diagnostic(operation: Operation, diagnostic: &Diagnostic) -> io::Result<()> {
    writeln!(
        io::stderr(),
        "ish: {}: {}: {}",
        operation_name(operation),
        escape_bytes(diagnostic.path.as_os_str().as_bytes()),
        escape_bytes(diagnostic.message.as_bytes())
    )
}

fn render_operand(
    operation: Operation,
    outcome: &OperandOutcome,
    dry_run: bool,
) -> Option<String> {
    let source = escape_bytes(outcome.source.as_os_str().as_bytes());
    let destination = escape_bytes(outcome.destination.as_os_str().as_bytes());
    let action = match outcome.state {
        OperandState::Planned if dry_run => {
            return Some(format!(
                "would {} {source} to {destination}",
                verb(operation)
            ));
        }
        OperandState::Copied if operation == Operation::Copy => "copied",
        OperandState::Moved if operation == Operation::Move => "moved",
        OperandState::SameEntry => return Some(format!("{source} and {destination} are the same entry")),
        OperandState::Skipped => return Some(format!("skipped existing target {destination}")),
        OperandState::DestinationPublishedSourceRetained => {
            return Some(format!("published {destination}; retained source {source}"));
        }
        OperandState::DestinationPartiallyPublished => {
            return Some(format!("partially published {destination}; retained source {source}"));
        }
        OperandState::SourcePartiallyRetired => {
            return Some(format!("partially retired source {source}; inspect retained entries"));
        }
        OperandState::SourceRemovedSyncFailed => {
            return Some(format!("removed source {source}; source parent sync failed"));
        }
        OperandState::Cancelled => return Some(format!("cancelled; retained source {source}")),
        OperandState::Planned
        | OperandState::Copied
        | OperandState::Moved
        | OperandState::FailedBeforePublication => return None,
    };
    Some(format!("{action} {source} to {destination}"))
}

fn render_partial(outcome: &OperandOutcome) -> String {
    let source = escape_bytes(outcome.source.as_os_str().as_bytes());
    let destination = escape_bytes(outcome.destination.as_os_str().as_bytes());
    match outcome.state {
        OperandState::DestinationPublishedSourceRetained => {
            format!("published {destination}; retained source {source}")
        }
        OperandState::DestinationPartiallyPublished => {
            format!("partially published {destination}; retained source {source}")
        }
        OperandState::SourcePartiallyRetired => {
            format!("partially retired source {source}; inspect retained entries")
        }
        OperandState::SourceRemovedSyncFailed => {
            format!("removed source {source}; source parent sync failed")
        }
        OperandState::Cancelled => format!("cancelled; retained source {source}"),
        _ => String::new(),
    }
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b' '..=b'~' if byte != b'\\' => escaped.push(byte as char),
            b'\\' => escaped.push_str("\\\\"),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\x{byte:02x}");
            }
        }
    }
    escaped
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Copy => "cp",
        Operation::Move => "mv",
    }
}

fn verb(operation: Operation) -> &'static str {
    match operation {
        Operation::Copy => "copy",
        Operation::Move => "move",
    }
}

fn usage(operation: Operation) -> &'static str {
    match operation {
        Operation::Copy => "usage: cp [OPTIONS] SOURCE DESTINATION\n       cp [OPTIONS] SOURCE... DIRECTORY\n       cp [OPTIONS] --into DIRECTORY SOURCE...\n",
        Operation::Move => "usage: mv [OPTIONS] SOURCE DESTINATION\n       mv [OPTIONS] SOURCE... DIRECTORY\n       mv [OPTIONS] --into DIRECTORY SOURCE...\n",
    }
}

fn help(operation: Operation) -> String {
    let recursion = match operation {
        Operation::Copy => "  -r, -R                   Accepted; directory recursion is automatic\n",
        Operation::Move => "",
    };
    format!(
        "{}\n{}\nOPTIONS:\n{}  --merge                   Merge directory contents\n  --replace                 Replace supported leaf targets\n  -n, --no-clobber           Skip existing targets\n  --dry-run                 Report planned actions\n  --durable                 Synchronize published copy data\n  --cross-file-systems      Traverse nested file systems\n  -j, --jobs N              Use N transfer workers (1..=32)\n  -v, --verbose              Report completed operations\n  --no-progress             Suppress terminal progress\n  -h, --help                 Show this help\n",
        usage(operation).trim_end(),
        "",
        recursion
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::os::unix::ffi::OsStringExt;

    fn context() -> Context {
        Context {
            cwd: PathBuf::from("/work"),
            home: Some(PathBuf::from("/home/user")),
            scratch: PathBuf::from("/scratch"),
        }
    }

    fn arguments(arguments: &[&[u8]]) -> Vec<OsString> {
        arguments
            .iter()
            .map(|argument| OsString::from_vec(argument.to_vec()))
            .collect()
    }

    #[test]
    fn into_options_and_raw_names_are_preserved() {
        let arguments = arguments(&[
            b"--into",
            b"target",
            b"--merge",
            b"--replace",
            b"--dry-run",
            b"--durable",
            b"--cross-file-systems",
            b"--jobs",
            b"3",
            b"-v",
            b"--no-progress",
            b"-r",
            b"-R",
            b"source-\xff",
            b"second",
        ]);

        let ParsedCommand::Run(invocation) =
            parse(Operation::Copy, &arguments, context()).expect("valid request")
        else {
            panic!("expected a transfer request");
        };

        assert_eq!(invocation.request.operation, Operation::Copy);
        assert_eq!(invocation.request.context.cwd, PathBuf::from("/work"));
        assert_eq!(invocation.request.context.home, Some(PathBuf::from("/home/user")));
        assert_eq!(invocation.request.context.scratch, PathBuf::from("/scratch"));
        assert!(invocation.request.options.merge);
        assert!(invocation.request.options.dry_run);
        assert!(invocation.request.options.durable);
        assert!(invocation.request.options.cross_file_systems);
        assert_eq!(invocation.request.options.collision, Collision::Replace);
        assert_eq!(invocation.request.options.jobs.map(Jobs::get), Some(3));
        assert!(invocation.verbose);
        assert!(invocation.no_progress);
        assert!(matches!(
            invocation.request.operands,
            Operands::Into { directory, first, rest }
                if directory == Path::new("target")
                    && first.as_os_str().as_bytes() == b"source-\xff"
                    && rest == vec![PathBuf::from("second")]
        ));
    }

    #[test]
    fn exact_mapping_accepts_leading_dash_paths_after_separator() {
        let arguments = arguments(&[b"--", b"-source", b"-destination"]);
        let ParsedCommand::Run(invocation) =
            parse(Operation::Move, &arguments, context()).expect("valid request")
        else {
            panic!("expected a transfer request");
        };

        assert!(matches!(
            invocation.request.operands,
            Operands::Exact { source, destination }
                if source == Path::new("-source")
                    && destination == Path::new("-destination")
        ));
    }

    #[test]
    fn implicit_multi_source_maps_last_operand_to_directory() {
        for operation in [Operation::Copy, Operation::Move] {
            let arguments = arguments(&[b"a", b"b", b"target"]);
            let ParsedCommand::Run(invocation) =
                parse(operation, &arguments, context()).expect("valid request")
            else {
                panic!("expected a transfer request");
            };
            assert!(matches!(
                invocation.request.operands,
                Operands::Into { directory, first, rest }
                    if directory == Path::new("target")
                        && first == Path::new("a")
                        && rest == vec![PathBuf::from("b")]
            ));
        }

        let arguments = arguments(&[b"--", b"-a", b"-b", b"-target"]);
        let ParsedCommand::Run(invocation) =
            parse(Operation::Move, &arguments, context()).expect("valid request")
        else {
            panic!("expected a transfer request");
        };
        assert!(matches!(
            invocation.request.operands,
            Operands::Into { directory, first, rest }
                if directory == Path::new("-target")
                    && first == Path::new("-a")
                    && rest == vec![PathBuf::from("-b")]
        ));
    }

    #[test]
    fn invalid_options_and_mapping_are_syntax_errors() {
        for arguments in [
            arguments(&[b"--unknown", b"source", b"destination"]),
            arguments(&[b"--jobs", b"0", b"source", b"destination"]),
            arguments(&[b"--jobs", b"33", b"source", b"destination"]),
            arguments(&[b"--replace", b"-n", b"source", b"destination"]),
            arguments(&[]),
            arguments(&[b"source"]),
            arguments(&[b"--into", b"target"]),
        ] {
            assert!(parse(Operation::Copy, &arguments, context()).is_err());
        }

        let non_utf8_option = arguments(&[b"-\xff", b"source", b"destination"]);
        assert!(parse(Operation::Copy, &non_utf8_option, context()).is_err());
    }

    #[test]
    fn help_does_not_hide_invalid_options() {
        for arguments in [
            arguments(&[b"--help", b"--unknown"]),
            arguments(&[b"--help", b"--jobs", b"0"]),
            arguments(&[b"--help", b"--replace", b"-n"]),
        ] {
            assert!(parse(Operation::Copy, &arguments, context()).is_err());
        }
        assert!(matches!(parse(Operation::Copy, &arguments(&[b"--help"]), context()), Ok(ParsedCommand::Help)));
    }

    #[test]
    fn recursive_flags_are_redundant_for_copy_and_rejected_for_move() {
        let arguments = arguments(&[b"-r", b"-R", b"source", b"destination"]);
        assert!(matches!(
            parse(Operation::Copy, &arguments, context()),
            Ok(ParsedCommand::Run(_))
        ));
        assert!(parse(Operation::Move, &arguments, context()).is_err());
    }

    #[test]
    fn rendered_paths_escape_terminal_control_and_invalid_bytes() {
        assert_eq!(
            escape_bytes(b"line\n\x1b\xff\\name"),
            "line\\x0a\\x1b\\xff\\\\name"
        );
    }

    #[test]
    fn dry_run_reports_planned_mapping_without_verbose_mode() {
        let outcome = OperandOutcome {
            source: PathBuf::from("report.txt"),
            destination: PathBuf::from("archive/report.txt"),
            state: OperandState::Planned,
        };
        assert_eq!(
            render_operand(Operation::Copy, &outcome, true).as_deref(),
            Some("would copy report.txt to archive/report.txt")
        );
    }

    #[test]
    fn partial_move_outcomes_name_published_destinations_and_retired_sources() {
        for (state, expected) in [
            (
                OperandState::DestinationPublishedSourceRetained,
                "published target/tree; retained source source/tree",
            ),
            (
                OperandState::DestinationPartiallyPublished,
                "partially published target/tree; retained source source/tree",
            ),
            (
                OperandState::SourcePartiallyRetired,
                "partially retired source source/tree; inspect retained entries",
            ),
            (
                OperandState::SourceRemovedSyncFailed,
                "removed source source/tree; source parent sync failed",
            ),
        ] {
            let outcome = OperandOutcome {
                source: PathBuf::from("source/tree"),
                destination: PathBuf::from("target/tree"),
                state,
            };
            assert_eq!(render_partial(&outcome), expected);
        }
    }
}
