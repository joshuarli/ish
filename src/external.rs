use crate::builtin::{self, ExternalCommand};
use crate::{fs_command, history, term};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;

pub struct StoppedJob {
    pub pgid: i32,
    pub cmd: String,
    pub termios: term::Termios,
}

thread_local! {
    static STOPPED_JOB: RefCell<Option<StoppedJob>> = const { RefCell::new(None) };
    static JOB_SLOT_TAKEN: Cell<bool> = const { Cell::new(false) };
}

pub fn begin_command(job_slot_taken: bool) {
    JOB_SLOT_TAKEN.with(|taken| taken.set(job_slot_taken));
}

pub fn take_stopped_job() -> Option<StoppedJob> {
    STOPPED_JOB.with(|cell| cell.borrow_mut().take())
}

fn env_pair_value(
    env_pairs: &[(epsh::shell_bytes::ShellBytes, epsh::shell_bytes::ShellBytes)],
    name: &str,
) -> Option<OsString> {
    env_pairs
        .iter()
        .find_map(|(k, v)| (k.as_bytes() == name.as_bytes()).then(|| v.to_os_string()))
}

pub fn make_external_handler(
    shell_pid: i32,
    history_log: std::path::PathBuf,
    fs_default_scratch: std::path::PathBuf,
) -> epsh::eval::ExternalHandler {
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
                    return fs_command::run(operation, &arguments, context);
                }
                Some(ExternalCommand::List) => {
                    let l_args: Vec<_> = args[1..].iter().map(|arg| arg.to_path_buf()).collect();
                    let status = builtin::list_directory(&l_args);
                    return Ok(epsh::error::ExitStatus::from(status));
                }
                Some(ExternalCommand::ClearScreen) => {
                    print!("\x1b[H\x1b[2J");
                    return Ok(epsh::error::ExitStatus::SUCCESS);
                }
                Some(ExternalCommand::History) => {
                    if args.len() > 1 {
                        let argument = args[1].to_shell_string();
                        if args.len() == 2 && matches!(argument.as_str(), "-h" | "--help") {
                            println!("Usage: history [reset]");
                            return Ok(epsh::error::ExitStatus::SUCCESS);
                        }
                        eprintln!("ish: history: storage subcommands require a standalone command");
                        return Ok(epsh::error::ExitStatus::FAILURE);
                    }
                    match history::render_history_log(&history_log) {
                        Ok(content) => {
                            print!("{content}");
                            return Ok(epsh::error::ExitStatus::SUCCESS);
                        }
                        Err(e) => {
                            eprintln!("ish: history: {e}");
                            return Ok(epsh::error::ExitStatus::FAILURE);
                        }
                    }
                }
                None => {}
            }

            spawn_external(args, env_pairs, shell_pid)
        },
    )
}

fn spawn_external(
    args: &[epsh::shell_bytes::ShellBytes],
    env_pairs: &[(epsh::shell_bytes::ShellBytes, epsh::shell_bytes::ShellBytes)],
    shell_pid: i32,
) -> epsh::error::Result<epsh::error::ExitStatus> {
    let name = args[0].to_shell_string();
    let is_main = rustix::process::getpid().as_raw_pid() == shell_pid;

    let mut cmd = std::process::Command::new(args[0].to_os_string());
    cmd.args(args[1..].iter().map(|arg| arg.to_os_string()));
    if let Some(cwd) = epsh::eval::external_command_cwd() {
        cmd.current_dir(cwd);
    }

    cmd.env_clear();
    for (k, v) in env_pairs {
        cmd.env(k.to_os_string(), v.to_os_string());
    }

    // SAFETY: the child only changes its process group and signal dispositions before exec.
    unsafe {
        cmd.pre_exec(move || {
            if is_main {
                let _ = rustix::process::setpgid(None, None);
            }
            crate::signal::restore_defaults();
            Ok(())
        });
    }

    match cmd.spawn() {
        Ok(mut child) => {
            let child_id = child.id() as i32;
            let child_pid = unsafe { rustix::process::Pid::from_raw_unchecked(child_id) };

            if is_main {
                if let Err(error) = rustix::process::setpgid(Some(child_pid), Some(child_pid))
                    && !matches!(error, rustix::io::Errno::ACCESS | rustix::io::Errno::SRCH)
                {
                    eprintln!("ish: cannot set process group for {name}: {error}");
                }
                if let Err(error) = crate::job::hand_terminal_to(child_pid) {
                    eprintln!("ish: cannot hand the terminal to {name}: {error}");
                }

                let waited = loop {
                    let waited = crate::job::retry_interrupted(|| {
                        rustix::process::waitpid(
                            Some(child_pid),
                            rustix::process::WaitOptions::UNTRACED,
                        )
                    });

                    crate::job::reclaim_terminal();

                    if let Ok(Some((_, status))) = &waited
                        && status.stopped()
                        && JOB_SLOT_TAKEN.with(|taken| taken.get())
                        && let Ok(stopped_termios) = term::save_termios()
                    {
                        eprintln!(
                            "\nish: a job is already suspended; resume it with fg before suspending another"
                        );
                        if let Err(error) =
                            crate::job::continue_in_foreground(child_pid, &stopped_termios)
                        {
                            eprintln!("ish: cannot continue {name}: {error}");
                            crate::job::kill_group(child_pid);
                        }
                        continue;
                    }
                    break waited;
                };

                let status = match waited {
                    Ok(Some((_, status))) => status,
                    Ok(None) => {
                        term::restore_startup_termios();
                        return Ok(epsh::error::ExitStatus::from(crate::job::WAIT_FAILURE_STATUS));
                    }
                    Err(error) => {
                        eprintln!("ish: cannot wait for {name}: {error}");
                        term::restore_startup_termios();
                        return Ok(epsh::error::ExitStatus::from(crate::job::WAIT_FAILURE_STATUS));
                    }
                };

                if status.stopped() {
                    let saved_termios = match term::save_termios() {
                        Ok(termios) => termios,
                        Err(error) => {
                            eprintln!("ish: cannot save terminal attributes for {name}: {error}");
                            crate::job::kill_group(child_pid);
                            let _ = crate::job::retry_interrupted(|| {
                                rustix::process::waitpid(
                                    Some(child_pid),
                                    rustix::process::WaitOptions::empty(),
                                )
                            });
                            term::restore_startup_termios();
                            return Ok(epsh::error::ExitStatus::from(
                                crate::job::WAIT_FAILURE_STATUS,
                            ));
                        }
                    };
                    let cmd = args[0].to_shell_string();
                    STOPPED_JOB.with(|cell| {
                        *cell.borrow_mut() = Some(StoppedJob {
                            pgid: child_id,
                            cmd,
                            termios: saved_termios,
                        });
                    });
                }
                term::restore_startup_termios();
                if status.stopped() {
                    Err(epsh::error::ShellError::Stopped {
                        pid: child_id,
                        pgid: child_id,
                    })
                } else if status.exited() {
                    Ok(epsh::error::ExitStatus::from(
                        status.exit_status().unwrap_or(1),
                    ))
                } else if status.signaled() {
                    Ok(epsh::error::ExitStatus::from(
                        128 + status.terminating_signal().unwrap_or(0),
                    ))
                } else {
                    Ok(epsh::error::ExitStatus::FAILURE)
                }
            } else {
                match child.wait() {
                    Ok(s) => Ok(epsh::error::ExitStatus::from(s.code().unwrap_or(128))),
                    Err(e) => Err(epsh::error::ShellError::Io(e)),
                }
            }
        }
        Err(e) => {
            if let Some(code) = e.raw_os_error() {
                let cwd = std::env::current_dir().unwrap_or_default();
                let path_env = env_pair_value(env_pairs, "PATH")
                    .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
                if let Some(msg) = epsh::eval::bad_interpreter_message_os(&args[0].to_os_string(), code, &cwd, &path_env) {
                    use std::io::Write;
                    use std::os::unix::ffi::OsStrExt;
                    let mut stderr = std::io::stderr().lock();
                    stderr.write_all(msg.as_bytes())?;
                    stderr.write_all(b"\n")?;
                    return Ok(epsh::error::ExitStatus::NOT_EXECUTABLE);
                }
            }
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("{name}: not found");
                Ok(epsh::error::ExitStatus::NOT_FOUND)
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!("{name}: permission denied");
                Ok(epsh::error::ExitStatus::NOT_EXECUTABLE)
            } else {
                Err(epsh::error::ShellError::Io(e))
            }
        }
    }
}
