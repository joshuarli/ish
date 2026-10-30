use crate::builtin::{self, ExternalCommand};
use crate::{fs_command, history};
use crate::report;
use std::ffi::OsString;

fn env_pair_value(
    env_pairs: &[(epsh::shell_bytes::ShellBytes, epsh::shell_bytes::ShellBytes)],
    name: &str,
) -> Option<OsString> {
    env_pairs
        .iter()
        .find_map(|(k, v)| (k.as_bytes() == name.as_bytes()).then(|| v.to_os_string()))
}

pub fn make_external_dispatcher(
    history_log: std::path::PathBuf,
    fs_default_scratch: std::path::PathBuf,
) -> epsh::eval::ExternalDispatcher {
    Box::new(
        move |args: &[epsh::shell_bytes::ShellBytes],
              env_pairs: &[(epsh::shell_bytes::ShellBytes, epsh::shell_bytes::ShellBytes)]| {
            let name = args[0].to_shell_string();

            let external = builtin::lookup(&name).and_then(builtin::Builtin::external_command);
            match external {
                Some(command @ (ExternalCommand::Copy | ExternalCommand::Move)) => {
                    let cwd = epsh::eval::external_command_cwd().ok_or_else(|| {
                        epsh::error::ShellError::Io(std::io::Error::other("external command cwd was unavailable"))
                    })?;
                    let context = ish_fs::Context {
                        cwd,
                        home: env_pair_value(env_pairs, "HOME").map(std::path::PathBuf::from),
                        scratch: env_pair_value(env_pairs, "TMPDIR")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|| fs_default_scratch.clone()),
                    };
                    let operation = match command {
                        ExternalCommand::Copy => ish_fs::Operation::Copy,
                        _ => ish_fs::Operation::Move,
                    };
                    let arguments = args[1..].iter()
                        .map(epsh::shell_bytes::ShellBytes::to_os_string)
                        .collect::<Vec<_>>();
                    return fs_command::run(operation, &arguments, context).map(epsh::eval::ExternalDispatch::Handled);
                }
                Some(command @ (ExternalCommand::List | ExternalCommand::ListHidden)) => {
                    let l_args: Vec<_> = args[1..].iter().map(|arg| arg.to_path_buf()).collect();
                    let show_hidden = matches!(command, ExternalCommand::ListHidden);
                    let status = builtin::list_directory(&l_args, show_hidden);
                    return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::from(status)));
                }
                Some(ExternalCommand::ClearScreen) => {
                    print!("\x1b[H\x1b[2J");
                    return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::SUCCESS));
                }
                Some(ExternalCommand::History) => {
                    if args.len() > 1 {
                        let argument = args[1].to_shell_string();
                        if args.len() == 2 && matches!(argument.as_str(), "-h" | "--help") {
                            println!("Usage: history [reset]");
                            return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::SUCCESS));
                        }
                        report!("ish: history: storage subcommands require a standalone command");
                        return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::FAILURE));
                    }
                    match history::render_history_log(&history_log) {
                        Ok(content) => {
                            print!("{content}");
                            return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::SUCCESS));
                        }
                        Err(e) => {
                            report!("ish: history: {e}");
                            return Ok(epsh::eval::ExternalDispatch::Handled(epsh::error::ExitStatus::FAILURE));
                        }
                    }
                }
                None => {}
            }

            Ok(epsh::eval::ExternalDispatch::NotHandled)
        },
    )
}
