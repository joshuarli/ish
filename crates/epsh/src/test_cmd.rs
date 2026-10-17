use crate::error::ExitStatus;
use crate::shell_bytes::ShellBytes;
use crate::sys;

/// Full POSIX test/[ implementation with compound expressions.
pub(crate) fn test_eval(args: &[&str]) -> ExitStatus {
    if args.is_empty() {
        return ExitStatus::FAILURE;
    }
    let mut pos = 0;
    let result = test_or(args, &mut pos);
    // If there are unconsumed arguments, it's a syntax error
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

fn test_or(args: &[&str], pos: &mut usize) -> bool {
    let mut result = test_and(args, pos);
    while *pos < args.len() && args[*pos] == "-o" {
        *pos += 1;
        let right = test_and(args, pos);
        result = result || right;
    }
    result
}

fn test_and(args: &[&str], pos: &mut usize) -> bool {
    let mut result = test_not(args, pos);
    while *pos < args.len() && args[*pos] == "-a" {
        *pos += 1;
        let right = test_not(args, pos);
        result = result && right;
    }
    result
}

fn test_not(args: &[&str], pos: &mut usize) -> bool {
    if *pos < args.len() && args[*pos] == "!" {
        *pos += 1;
        !test_primary(args, pos)
    } else {
        test_primary(args, pos)
    }
}

fn test_primary(args: &[&str], pos: &mut usize) -> bool {
    if *pos >= args.len() {
        return false;
    }

    // Parenthesized expression
    if args[*pos] == "(" {
        *pos += 1;
        let result = test_or(args, pos);
        if *pos < args.len() && args[*pos] == ")" {
            *pos += 1;
        }
        return result;
    }

    // Binary operators (check if next token is binary op)
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
                // File comparison operators
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
                let lm = std::fs::metadata(ShellBytes::from_str_lossless(left).to_path_buf()).ok();
                let rm = std::fs::metadata(ShellBytes::from_str_lossless(right).to_path_buf()).ok();
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

    // Unary operators — only match if there's a following operand
    // (and the operand isn't a closing paren or binary op)
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
            ShellBytes::from_str_lossless(p).to_path_buf().exists()
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
            ShellBytes::from_str_lossless(p).to_path_buf().is_file()
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
            ShellBytes::from_str_lossless(p).to_path_buf().is_dir()
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
            ShellBytes::from_str_lossless(p)
                .to_path_buf()
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
            match std::fs::metadata(ShellBytes::from_str_lossless(p).to_path_buf()) {
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
            std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
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
            let fd = fd.parse::<i32>().unwrap_or(1);
            unsafe { rustix::termios::isatty(std::os::fd::BorrowedFd::borrow_raw(fd)) }
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
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
            std::fs::metadata(p)
                .map(|m| m.mode() & 0o1000 != 0)
                .unwrap_or(false)
        }
        _ => {
            // Bare string: true if non-empty
            *pos += 1;
            !op.is_empty()
        }
    }
}
