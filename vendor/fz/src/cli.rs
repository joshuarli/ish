use std::borrow::Cow;
use std::ffi::{c_int, OsString};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use crate::terminal;

struct Options {
    lines: usize,
    prompt: Cow<'static, [u8]>,
    query: Vec<u8>,
    tty: Cow<'static, Path>,
    show_scores: bool,
    show_info: bool,
    multi: bool,
    filter: Option<Vec<u8>>,
    delimiter: u8,
    workers: usize,
}

pub const HELP: &str = "Usage: fz [OPTION]...
 -l, --lines=LINES        Results to show (default 10; minimum 3; or max)
 -p, --prompt=PROMPT      Input prompt (default '> ')
 -q, --query=QUERY        Initial search string
 -e, --show-matches=QUERY Output sorted matches without a terminal
 -t, --tty=TTY            Terminal device (default /dev/tty)
 -s, --show-scores        Show match scores
 -0, --read-null          Read NUL-delimited candidates
 -j, --workers=NUM        Maximum workers (default up to 4; small searches use 1)
 -i, --show-info          Show match count
 -m, --multi               Mark multiple candidates (Down, Space, Enter)
 -h, --help              Display this help
 -v, --version           Display version
";

impl Options {
    fn parse(mut args: impl Iterator<Item = Vec<u8>>) -> Result<Option<Self>, String> {
        let mut options = Self {
            lines: 10, prompt: Cow::Borrowed(b"> "), query: Vec::new(),
            tty: Cow::Borrowed(Path::new("/dev/tty")), show_scores: false, show_info: false,
            multi: false, filter: None, delimiter: b'\n', workers: 0,
        };
        while let Some(arg) = args.next() {
            if arg == b"--" {
                if args.next().is_some() { return Err("Unexpected positional argument".into()); }
                break;
            }
            if !arg.starts_with(b"-") || arg == b"-" {
                return Err("Unexpected positional argument".into());
            }
            let long_key;
            let (keys, attached) = if let Some(long) = arg.strip_prefix(b"--") {
                let end = long.iter().position(|&ch| ch == b'=').unwrap_or(long.len());
                long_key = match &long[..end] {
                    b"lines" => b'l', b"prompt" => b'p', b"query" => b'q',
                    b"show-matches" => b'e', b"tty" => b't', b"show-scores" => b's',
                    b"read-null" => b'0', b"workers" => b'j', b"show-info" => b'i',
                    b"multi" => b'm',
                    b"help" => b'h', b"version" => b'v', _ => b'?',
                };
                (&[long_key][..], (end < long.len()).then(|| &long[end + 1..]))
            } else {
                (&arg[1..], None)
            };
            for (index, &key) in keys.iter().enumerate() {
                if b"lpqetj".contains(&key) {
                    let value: Cow<'_, [u8]> = if let Some(value) = attached {
                        Cow::Borrowed(value)
                    } else if index + 1 < keys.len() {
                        Cow::Borrowed(&keys[index + 1..])
                    } else if let Some(value) = args.next() {
                        Cow::Owned(value)
                    } else {
                        eprint!("{HELP}");
                        return Ok(None);
                    };
                    match key {
                        b'l' => {
                            options.lines = if value.as_ref() == b"max" { usize::MAX } else {
                                std::str::from_utf8(value.as_ref()).ok().and_then(|s| s.parse().ok())
                                    .filter(|&n| n >= 3).ok_or("--lines must be an integer >= 3 or max")?
                            };
                        }
                        b'p' => options.prompt = Cow::Owned(value.into_owned()),
                        b'q' => options.query = value.into_owned(),
                        b'e' => options.filter = Some(value.into_owned()),
                        b't' => options.tty = Cow::Owned(PathBuf::from(OsString::from_vec(value.into_owned()))),
                        b'j' => {
                            options.workers = std::str::from_utf8(value.as_ref()).ok().and_then(|s| s.parse::<u32>().ok())
                                .ok_or("--workers must be an unsigned integer")? as usize;
                        }
                        _ => unreachable!(),
                    }
                    break;
                }
                if attached.is_some() {
                    eprint!("{HELP}");
                    return Ok(None);
                }
                match key {
                    b's' => options.show_scores = true,
                    b'0' => options.delimiter = 0,
                    b'i' => options.show_info = true,
                    b'm' => options.multi = true,
                    b'v' => {
                        println!("fz {} (fzy port; © 2014-2025 John Hawthorn)", env!("CARGO_PKG_VERSION"));
                        return Ok(None);
                    }
                    _ => {
                        eprint!("{HELP}");
                        return Ok(None);
                    }
                }
            }
        }
        if options.multi && options.filter.is_some() {
            return Err("--multi cannot be combined with --show-matches".into());
        }
        Ok(Some(options))
    }
}

// fzy skips empty records. In newline mode the first NUL terminates the input
// visible to its C string tokenizer; in NUL mode newlines are candidate bytes.
fn read_candidates(delimiter: u8, terminal: Option<&terminal::Terminal>) -> io::Result<crate::Candidates> {
    let mut input = Vec::new();
    // A File reader uses the remaining file length as its allocation hint.
    // This avoids geometric input growth when stdin is a redirected file,
    // while pipes retain streaming reads. No buffered stdin reads precede it.
    let mut source = std::fs::File::from(rustix::io::dup(rustix::stdio::stdin())?);
    if let Some(terminal) = terminal {
        if let Ok(metadata) = source.metadata() { input.reserve_exact(usize::try_from(metadata.len()).unwrap_or(0)); }
        let mut buffer = [0u8; 64 * 1024];
        loop {
            terminal.wait_candidate_input(&source)?;
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => input.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    } else { source.read_to_end(&mut input)?; }
    Ok(crate::Candidates::from_input(input, delimiter))
}

fn run_options(options: Options, claim_foreground: bool, input: Option<std::fs::File>) -> io::Result<bool> {
    if let Some(query) = &options.filter {
        let mut choices = crate::Choices::from_candidates(read_candidates(options.delimiter, None)?);
        choices.set_workers(options.workers);
        choices.search(query);
        choices.rank_all();
        let mut output = io::BufWriter::new(io::stdout().lock());
        for index in 0..choices.available() {
            if options.show_scores { write!(output, "{:.6}\t", choices.getscore(index).unwrap())?; }
            output.write_all(choices.get(index).unwrap())?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
        return Ok(true);
    }

    // Set terminal input mode before consuming a pipe so early keystrokes are
    // queued without canonical editing or CR-to-NL translation. When stdin is
    // itself a terminal, retain canonical input until its candidate EOF.
    let (mut terminal, candidates) = if io::stdin().is_terminal() {
        let candidates = read_candidates(options.delimiter, None)?;
        (terminal::Terminal::open(&options.tty, claim_foreground, input)?, candidates)
    } else {
        let terminal = terminal::Terminal::open(&options.tty, claim_foreground, input)?;
        { let candidates = read_candidates(options.delimiter, Some(&terminal))?; (terminal, candidates) }
    };
    let mut picker = crate::Picker::new(candidates, crate::PickerConfig {
        lines: options.lines, prompt: options.prompt.into_owned(), query: options.query,
        show_scores: options.show_scores, show_info: options.show_info, multi: options.multi,
        workers: options.workers,
    });
    let selection = terminal.run(&mut picker)?;
    drop(terminal);
    let mut output = io::stdout().lock();
    match selection {
        crate::Outcome::Accepted(ids) => {
            for id in ids {
                output.write_all(picker.source(id).expect("accepted source exists"))?;
                output.write_all(b"\n")?;
            }
        }
        crate::Outcome::Query(query) => { output.write_all(&query)?; output.write_all(b"\n")?; }
        crate::Outcome::Cancelled | crate::Outcome::NoMatch => return Ok(false),
    }
    output.flush()?;
    Ok(true)
}

/// Run the standalone CLI contract in a process whose terminal and signals it owns.
pub fn run(args: impl Iterator<Item = Vec<u8>>) -> c_int { dispatch(args, false, None) }

/// Run in a fresh embedding worker, temporarily taking and restoring the
/// controlling terminal's foreground group for interactive substitutions.
pub fn run_in_foreground(args: impl Iterator<Item = Vec<u8>>, input: Option<std::fs::File>) -> c_int { dispatch(args, true, input) }

fn dispatch(args: impl Iterator<Item = Vec<u8>>, claim_foreground: bool, input: Option<std::fs::File>) -> c_int {
    match Options::parse(args) {
        Ok(None) => 0,
        Ok(Some(options)) => {
            let multi = options.multi;
            match run_options(options, claim_foreground, input) {
                Ok(true) => 0,
                Ok(false) => 1,
                Err(error) => {
                    if let Some(signal) = error.get_ref().and_then(|cause| cause.downcast_ref::<crate::termination::Interrupted>()) { return 128 + signal.0; }
                    eprintln!("fz: {error}");
                    if multi { 2 } else { 1 }
                }
            }
        },
        Err(error) => {
            eprintln!("fz: {error}\n{HELP}");
            1
        }
    }
}
