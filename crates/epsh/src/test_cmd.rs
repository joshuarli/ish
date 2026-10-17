use crate::error::ExitStatus;
use crate::shell_bytes::ShellBytes;
use crate::sys;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

pub(crate) fn test_eval(args: &[&str], cwd: &Path) -> ExitStatus {
    if args.is_empty() {
        return ExitStatus::FAILURE;
    }
    let mut pos = 0;
    let result = test_or(args, &mut pos, cwd);
    if pos < args.len() {
        eprintln!("test: too many arguments");
        return ExitStatus::MISUSE;
    }
    if result {
        ExitStatus::SUCCESS
    } else {
        ExitStatus::FAILURE
    }
}

fn test_or(args: &[&str], pos: &mut usize, cwd: &Path) -> bool {
    let mut result = test_and(args, pos, cwd);
    while *pos < args.len() && args[*pos] == "-o" {
        *pos += 1;
        let right = test_and(args, pos, cwd);
        result = result || right;
    }
    result
}

fn test_and(args: &[&str], pos: &mut usize, cwd: &Path) -> bool {
    let mut result = test_not(args, pos, cwd);
    while *pos < args.len() && args[*pos] == "-a" {
        *pos += 1;
        let right = test_not(args, pos, cwd);
        result = result && right;
    }
    result
}

fn test_not(args: &[&str], pos: &mut usize, cwd: &Path) -> bool {
    if *pos < args.len() && args[*pos] == "!" {
        *pos += 1;
        !test_primary(args, pos, cwd)
    } else {
        test_primary(args, pos, cwd)
    }
}

fn test_primary(args: &[&str], pos: &mut usize, cwd: &Path) -> bool {
    if *pos >= args.len() {
        return false;
    }

    if args[*pos] == "(" {
        *pos += 1;
        let result = test_or(args, pos, cwd);
        if *pos < args.len() && args[*pos] == ")" {
            *pos += 1;
        }
        return result;
    }

    if *pos + 2 <= args.len() {
        let maybe_op = if *pos + 1 < args.len() {
            args[*pos + 1]
        } else {
            ""
        };
        match maybe_op {
            "=" | "==" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left == right;
            }
            "!=" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left != right;
            }
            "<" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left < right;
            }
            ">" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left > right;
            }
            "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                let left = args[*pos].parse::<i64>().unwrap_or(0);
                let op = args[*pos + 1];
                *pos += 2;
                let right = if *pos < args.len() {
                    let r = args[*pos].parse::<i64>().unwrap_or(0);
                    *pos += 1;
                    r
                } else {
                    *pos += 1;
                    0
                };
                return match op {
                    "-eq" => left == right,
                    "-ne" => left != right,
                    "-lt" => left < right,
                    "-le" => left <= right,
                    "-gt" => left > right,
                    "-ge" => left >= right,
                    _ => false,
                };
            }
            "-nt" | "-ot" | "-ef" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() {
                    let r = args[*pos];
                    *pos += 1;
                    r
                } else {
                    *pos += 1;
                    ""
                };
                let lm = std::fs::metadata(test_path(cwd, left)).ok();
                let rm = std::fs::metadata(test_path(cwd, right)).ok();
                return match maybe_op {
                    "-nt" => match (lm, rm) {
                        (Some(l), Some(r)) => l.modified().ok() > r.modified().ok(),
                        (Some(_), None) => true,
                        _ => false,
                    },
                    "-ot" => match (lm, rm) {
                        (Some(l), Some(r)) => l.modified().ok() < r.modified().ok(),
                        (None, Some(_)) => true,
                        _ => false,
                    },
                    "-ef" => {
                        use std::os::unix::fs::MetadataExt;
                        match (lm, rm) {
                            (Some(l), Some(r)) => l.dev() == r.dev() && l.ino() == r.ino(),
                            _ => false,
                        }
                    }
                    _ => false,
                };
            }
            _ => {}
        }
    }

    let op = args[*pos];
    let has_operand = *pos + 1 < args.len()
        && !matches!(
            args[*pos + 1],
            ")" | "-a" | "-o" | "=" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
        );
    match op {
        "-n" if has_operand => {
            *pos += 1;
            let s = args[*pos];
            *pos += 1;
            !s.is_empty()
        }
        "-z" if has_operand => {
            *pos += 1;
            let s = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            s.is_empty()
        }
        "-e" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            test_path(cwd, p).exists()
        }
        "-f" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            test_path(cwd, p).is_file()
        }
        "-d" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            test_path(cwd, p).is_dir()
        }
        "-L" | "-h" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            test_path(cwd, p)
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        }
        "-r" | "-w" | "-x" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            let uid = sys::getuid();
            match std::fs::metadata(test_path(cwd, p)) {
                Ok(m) => {
                    let mode = m.mode();
                    let is_owner = m.uid() == uid;
                    match op {
                        "-r" => {
                            if is_owner {
                                mode & 0o400 != 0
                            } else {
                                mode & 0o004 != 0
                            }
                        }
                        "-w" => {
                            if is_owner {
                                mode & 0o200 != 0
                            } else {
                                mode & 0o002 != 0
                            }
                        }
                        "-x" => {
                            if is_owner {
                                mode & 0o100 != 0
                            } else {
                                mode & 0o001 != 0
                            }
                        }
                        _ => false,
                    }
                }
                Err(_) => false,
            }
        }
        "-s" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        }
        "-t" if has_operand => {
            *pos += 1;
            let fd = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                "1"
            };
            let Ok(fd) = fd.parse::<i32>() else {
                return false;
            };
            sys::duplicate_fd_above(fd, 0)
                .is_ok_and(|fd| rustix::termios::isatty(fd.as_fd()))
        }
        "-p" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.file_type().is_fifo())
                .unwrap_or(false)
        }
        "-b" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.file_type().is_block_device())
                .unwrap_or(false)
        }
        "-c" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.file_type().is_char_device())
                .unwrap_or(false)
        }
        "-S" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        }
        "-u" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.mode() & 0o4000 != 0)
                .unwrap_or(false)
        }
        "-g" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.mode() & 0o2000 != 0)
                .unwrap_or(false)
        }
        "-k" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(test_path(cwd, p))
                .map(|m| m.mode() & 0o1000 != 0)
                .unwrap_or(false)
        }
        _ => {
            *pos += 1;
            !op.is_empty()
        }
    }
}

fn test_path(cwd: &Path, operand: &str) -> PathBuf {
    let path = ShellBytes::from_str_lossless(operand).to_path_buf();
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
