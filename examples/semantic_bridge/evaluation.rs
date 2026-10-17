use crate::semantic_bridge::protocol::{Family, Request, exact_object, require_bytes};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::ast::{Command, Program, Word, WordPart};
use epsh::error::{ExitStatus, ShellError};
use epsh::eval::{Shell, external_command_cwd};
use epsh::expand::remove_glob_escapes;
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const CALLBACK_PREFIX: &str = "__ish_evaluation_callback_";
const PRIME_COMMAND: &[u8] = b"__ish_evaluation_prime_status";
const MAX_SOURCE_BYTES: usize = 4096;
const MAX_OBSERVATIONS: usize = 64;
const MAX_EXPRESSION_DEPTH: usize = 32;
static NEXT_SANDBOX: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Observation {
    status: u8,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct InitialState {
    variables: Vec<(String, Vec<u8>)>,
    exported: Vec<String>,
    cwd: Vec<u8>,
    positional: Vec<Vec<u8>>,
    last_status: u8,
}

struct Options {
    errexit: bool,
    pipefail: bool,
}

struct Scratch {
    path: PathBuf,
    removed: bool,
}

struct OutputCapture {
    stdout: File,
    stderr: File,
    saved_stdout: OwnedFd,
    saved_stderr: OwnedFd,
    restored: bool,
}

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0x94d049bb133111eb));
    let state = initial_state(index);
    let (source, observations, options) = match index {
        0 => fixed_simple(&mut Vec::new()),
        1 => {
            let source = assignment_source("plain", b"assigned-only");
            (source, Vec::new(), Options { errexit: true, pipefail: false })
        }
        2 => {
            let mut observations = Vec::new();
            let source = external_source(
                &mut observations,
                7,
                Vec::new(),
                Vec::new(),
                &[("shared", b"temporary")],
                &[b"prefix".to_vec(), vec![0xff]],
            );
            (source, observations, Options { errexit: false, pipefail: false })
        }
        3 => {
            let mut observations = Vec::new();
            let first = external_source(
                &mut observations,
                0,
                b"first".to_vec(),
                Vec::new(),
                &[],
                &[b"one".to_vec()],
            );
            let second = external_source(
                &mut observations,
                4,
                Vec::new(),
                vec![b'e', 0xff],
                &[("plain", b"temporary")],
                &[b"two".to_vec()],
            );
            let source = join_source(&[
                assignment_source("plain", b"changed"),
                first,
                second,
            ], b"; ");
            (source, observations, Options { errexit: false, pipefail: false })
        }
        4 => {
            let mut observations = Vec::new();
            let left = external_source(
                &mut observations,
                9,
                b"left".to_vec(),
                Vec::new(),
                &[],
                &[b"left".to_vec()],
            );
            let right = external_source(
                &mut observations,
                0,
                b"right".to_vec(),
                Vec::new(),
                &[],
                &[b"right".to_vec()],
            );
            (join_source(&[left, right], b" && "), observations, Options { errexit: true, pipefail: false })
        }
        5 => {
            let mut observations = Vec::new();
            let left = external_source(
                &mut observations,
                2,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"left".to_vec()],
            );
            let right = external_source(
                &mut observations,
                3,
                b"or-right".to_vec(),
                Vec::new(),
                &[],
                &[b"right".to_vec()],
            );
            (join_source(&[left, right], b" || "), observations, Options { errexit: false, pipefail: false })
        }
        6 => {
            let mut observations = Vec::new();
            let command = external_source(
                &mut observations,
                9,
                b"negated".to_vec(),
                Vec::new(),
                &[],
                &[b"body".to_vec()],
            );
            (join_source(&[b"! ".to_vec(), command], b""), observations, Options { errexit: true, pipefail: false })
        }
        7 => {
            let mut observations = Vec::new();
            let command = external_source(
                &mut observations,
                13,
                b"child".to_vec(),
                Vec::new(),
                &[],
                &[b"body".to_vec()],
            );
            let body = join_source(&[assignment_source("plain", b"child"), command], b"; ");
            (join_source(&[b"(".to_vec(), body, b")".to_vec()], b""), observations, Options { errexit: false, pipefail: false })
        }
        8 => fixed_pipeline(&mut Vec::new(), &[7, 0], false, b"pipeline"),
        9 => fixed_pipeline(&mut Vec::new(), &[1, 7, 0], true, b"last"),
        10 => {
            let mut observations = Vec::new();
            let tested_failure = external_source(
                &mut observations,
                8,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"tested".to_vec()],
            );
            let skipped = external_source(
                &mut observations,
                0,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"skipped".to_vec()],
            );
            let untested_failure = external_source(
                &mut observations,
                6,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"halt".to_vec()],
            );
            let after_halt = external_source(
                &mut observations,
                0,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"unreachable".to_vec()],
            );
            let and = join_source(&[tested_failure, skipped], b" && ");
            let source = join_source(&[and, untested_failure, after_halt], b"; ");
            (source, observations, Options { errexit: true, pipefail: false })
        }
        11 => {
            let mut observations = Vec::new();
            let first = external_source(
                &mut observations,
                0,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"first".to_vec()],
            );
            let second = external_source(
                &mut observations,
                5,
                b"nested".to_vec(),
                Vec::new(),
                &[],
                &[b"second".to_vec()],
            );
            let body = join_source(&[assignment_source("counter", b"inner"), first, second], b"; ");
            (join_source(&[b"! (".to_vec(), body, b")".to_vec()], b""), observations, Options { errexit: true, pipefail: false })
        }
        12 => {
            let mut observations = Vec::new();
            let first = external_source(
                &mut observations,
                0,
                Vec::new(),
                Vec::new(),
                &[("shared", b"prefix-value")],
                &[b"and-left".to_vec()],
            );
            let second = external_source(
                &mut observations,
                5,
                Vec::new(),
                Vec::new(),
                &[],
                &[b"and-right".to_vec()],
            );
            let third = external_source(
                &mut observations,
                0,
                b"or-result".to_vec(),
                Vec::new(),
                &[],
                &[b"or-right".to_vec()],
            );
            let and = join_source(&[first, second], b" && ");
            (join_source(&[and, third], b" || "), observations, Options { errexit: false, pipefail: true })
        }
        _ => {
            let mut observations = Vec::new();
            let source = random_source(&mut random, 4, &mut observations);
            (
                source,
                observations,
                Options {
                    errexit: random.below(2) == 0,
                    pipefail: random.below(2) == 0,
                },
            )
        }
    };
    let input = make_input(&source, &observations, &options)?;
    let request = Request {
        id: format!("evaluation-{index:05}"),
        family: Family::Evaluation,
        source,
        state,
        fuel: 256,
        input,
    };
    validate_case(&request)?;
    Ok(request)
}

fn fixed_simple(observations: &mut Vec<Observation>) -> (Vec<u8>, Vec<Observation>, Options) {
    let source = external_source(
        observations,
        0,
        b"hello".to_vec(),
        Vec::new(),
        &[],
        &[b"quoted ' value".to_vec(), vec![0xff, b'x']],
    );
    (source, observations.clone(), Options { errexit: false, pipefail: false })
}

fn fixed_pipeline(
    observations: &mut Vec<Observation>,
    statuses: &[u8],
    pipefail: bool,
    final_stdout: &[u8],
) -> (Vec<u8>, Vec<Observation>, Options) {
    let mut commands = Vec::with_capacity(statuses.len());
    for (index, status) in statuses.iter().copied().enumerate() {
        let stdout = if index + 1 == statuses.len() {
            final_stdout.to_vec()
        } else {
            Vec::new()
        };
        commands.push(external_source(
            observations,
            status,
            stdout,
            Vec::new(),
            &[],
            &[format!("stage-{index}").into_bytes()],
        ));
    }
    (
        join_source(&commands, b" | "),
        observations.clone(),
        Options { errexit: false, pipefail },
    )
}

fn initial_state(index: usize) -> Value {
    let shared = if index.is_multiple_of(2) {
        b"exported".to_vec()
    } else {
        vec![b'e', 0xff]
    };
    json!({
        "variables": [
            ["counter", index.to_string().into_bytes()],
            ["plain", b"before".to_vec()],
            ["raw", vec![b'r', 0xff]],
            ["shared", shared.clone()],
        ],
        "exported": [["shared", shared]],
        "inherited": [],
        "cwd": [47],
        "positional": [b"positional".to_vec(), vec![b'p', 0xff]],
        "last_status": if index == 1 { 9 } else if index.is_multiple_of(6) { 11 } else { 0 },
    })
}

fn random_source(
    random: &mut Generator,
    depth: usize,
    observations: &mut Vec<Observation>,
) -> Vec<u8> {
    if depth == 0 {
        return random_leaf(random, observations, false);
    }
    match random.below(8) {
        0 => random_leaf(random, observations, false),
        1 => {
            let left = random_source(random, depth - 1, observations);
            let right = random_source(random, depth - 1, observations);
            join_source(&[left, right], b"; ")
        }
        2 => {
            let left = random_source(random, depth - 1, observations);
            let right = random_source(random, depth - 1, observations);
            join_source(&[wrap_source(left), right], b" && ")
        }
        3 => {
            let left = random_source(random, depth - 1, observations);
            let right = random_source(random, depth - 1, observations);
            join_source(&[wrap_source(left), right], b" || ")
        }
        4 => {
            let body = random_source(random, depth - 1, observations);
            join_source(&[b"! ".to_vec(), wrap_source(body)], b"")
        }
        5 => {
            let body = random_source(random, depth - 1, observations);
            wrap_source(body)
        }
        6 => random_pipeline(random, observations),
        _ => random_leaf(random, observations, true),
    }
}

fn random_leaf(
    random: &mut Generator,
    observations: &mut Vec<Observation>,
    outputless: bool,
) -> Vec<u8> {
    if !outputless && random.below(5) == 0 {
        let name = ["counter", "plain", "new_value"][random.below(3) as usize];
        let value = [b"changed".as_slice(), b"value".as_slice(), &[0xff, b'x']][random.below(3) as usize];
        return assignment_source(name, value);
    }
    let statuses = [0, 1, 2, 7, 17, 255];
    let status = statuses[random.below(statuses.len() as u64) as usize];
    let stdout = if outputless || random.below(3) == 0 {
        Vec::new()
    } else {
        [b"output".as_slice(), &[b'o', 0xff], b"tail"][random.below(3) as usize].to_vec()
    };
    let stderr = if outputless || random.below(5) != 0 {
        Vec::new()
    } else {
        b"diagnostic".to_vec()
    };
    let prefixes = if random.below(4) == 0 {
        vec![("shared", b"temporary".as_slice())]
    } else {
        Vec::new()
    };
    external_source(
        observations,
        status,
        stdout,
        stderr,
        &prefixes,
        &[b"argument".to_vec(), vec![0xff]],
    )
}

fn random_pipeline(random: &mut Generator, observations: &mut Vec<Observation>) -> Vec<u8> {
    let count = 2 + random.below(3) as usize;
    let mut commands = Vec::with_capacity(count);
    for index in 0..count {
        let statuses = [0, 1, 4, 9, 23];
        let status = statuses[random.below(statuses.len() as u64) as usize];
        let stdout = if index + 1 == count && random.below(2) == 0 {
            b"pipe-end".to_vec()
        } else {
            Vec::new()
        };
        commands.push(external_source(
            observations,
            status,
            stdout,
            Vec::new(),
            &[],
            &[format!("pipeline-{index}").into_bytes()],
        ));
    }
    join_source(&commands, b" | ")
}

fn external_source(
    observations: &mut Vec<Observation>,
    status: u8,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    prefixes: &[(&str, &[u8])],
    arguments: &[Vec<u8>],
) -> Vec<u8> {
    let index = observations.len();
    observations.push(Observation { status, stdout, stderr });
    let mut source = Vec::new();
    for (name, value) in prefixes {
        source.extend_from_slice(name.as_bytes());
        source.push(b'=');
        source.extend_from_slice(&quote_bytes(value));
        source.push(b' ');
    }
    source.extend_from_slice(callback_name(index).as_bytes());
    for argument in arguments {
        source.push(b' ');
        source.extend_from_slice(&quote_bytes(argument));
    }
    source
}

fn assignment_source(name: &str, value: &[u8]) -> Vec<u8> {
    let mut source = name.as_bytes().to_vec();
    source.push(b'=');
    source.extend_from_slice(&quote_bytes(value));
    source
}

fn quote_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut quoted = Vec::with_capacity(bytes.len() + 2);
    quoted.push(b'\'');
    for byte in bytes {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

fn join_source(parts: &[Vec<u8>], separator: &[u8]) -> Vec<u8> {
    let capacity = parts.iter().map(Vec::len).sum::<usize>()
        + separator.len().saturating_mul(parts.len().saturating_sub(1));
    let mut source = Vec::with_capacity(capacity);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            source.extend_from_slice(separator);
        }
        source.extend_from_slice(part);
    }
    source
}

fn wrap_source(source: Vec<u8>) -> Vec<u8> {
    join_source(&[b"(".to_vec(), source, b")".to_vec()], b"")
}

fn callback_name(index: usize) -> String {
    format!("{CALLBACK_PREFIX}{index:08}")
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    if request.family != Family::Evaluation {
        return Err(BridgeError::new("evaluation request has the wrong family"));
    }
    if request.source.is_empty() || request.source.len() > MAX_SOURCE_BYTES {
        return Err(BridgeError::new("evaluation source is empty or exceeds 4096 bytes"));
    }
    if request.source.contains(&0) {
        return Err(BridgeError::new("evaluation source contains a zero byte"));
    }
    let _state = decode_state(&request.state)?;
    let observations = decode_observations(&request.input)?;
    decode_options(&request.input)?;
    let parsed = parse_source(&request.source)?;
    let program = program_json(&parsed)?;
    if request.input["program"] != program {
        return Err(BridgeError::new(
            "evaluation expression does not match the production parser AST",
        ));
    }
    if observations.len() > MAX_OBSERVATIONS {
        return Err(BridgeError::new("evaluation observation count exceeds 64"));
    }
    let mut indices = Vec::new();
    collect_observation_indices(&program, &mut indices)?;
    if indices.len() != observations.len()
        || indices
            .iter()
            .copied()
            .ne(0..observations.len())
    {
        return Err(BridgeError::new(
            "evaluation observations do not correspond one-to-one with source leaves",
        ));
    }
    if expression_depth(&program) > MAX_EXPRESSION_DEPTH {
        return Err(BridgeError::new("evaluation expression exceeds 32 levels"));
    }
    if request.fuel < expression_depth(&program) as u64 {
        return Err(BridgeError::new("evaluation fuel is below the expression depth"));
    }
    validate_pipeline_observations(&program, &observations)?;
    Ok(())
}

fn decode_state(value: &Value) -> BridgeResult<InitialState> {
    exact_object(
        value,
        &[
            "variables",
            "exported",
            "inherited",
            "cwd",
            "positional",
            "last_status",
        ],
        "evaluation state",
    )?;
    let variables = decode_bindings(&value["variables"], "evaluation variables")?;
    let exported_pairs = decode_bindings(&value["exported"], "evaluation exports")?;
    if value["inherited"].as_array().is_none_or(|entries| !entries.is_empty()) {
        return Err(BridgeError::new(
            "evaluation source adapter requires an empty inherited environment",
        ));
    }
    let variable_map: std::collections::BTreeMap<_, _> = variables.iter().cloned().collect();
    for (name, exported_value) in &exported_pairs {
        if variable_map.get(name) != Some(exported_value) {
            return Err(BridgeError::new(
                "evaluation exports must match a shell variable value",
            ));
        }
    }
    let cwd = require_bytes(&value["cwd"], "evaluation cwd")?;
    let positional = value["positional"]
        .as_array()
        .ok_or_else(|| BridgeError::new("evaluation positional state is not an array"))?
        .iter()
        .map(|argument| require_bytes(argument, "evaluation positional argument"))
        .collect::<BridgeResult<Vec<_>>>()?;
    let last_status = value["last_status"]
        .as_u64()
        .filter(|status| *status < 256)
        .ok_or_else(|| BridgeError::new("evaluation last status is outside 0..255"))?;
    Ok(InitialState {
        variables,
        exported: exported_pairs.into_iter().map(|(name, _)| name).collect(),
        cwd,
        positional,
        last_status: last_status as u8,
    })
}

fn decode_bindings(value: &Value, context: &str) -> BridgeResult<Vec<(String, Vec<u8>)>> {
    let pairs = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an array")))?;
    let mut seen = HashSet::new();
    pairs
        .iter()
        .map(|pair| {
            let pair = pair
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| BridgeError::new(format!("{context} entry is not a pair")))?;
            let name = pair[0]
                .as_str()
                .ok_or_else(|| BridgeError::new(format!("{context} name is not text")))?;
            if !is_shell_name(name) {
                return Err(BridgeError::new(format!("{context} has an invalid shell name")));
            }
            if !seen.insert(name.to_string()) {
                return Err(BridgeError::new(format!("{context} has duplicate names")));
            }
            Ok((
                name.to_string(),
                require_bytes(&pair[1], context)?,
            ))
        })
        .collect()
}

fn is_shell_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn decode_observations(input: &Value) -> BridgeResult<Vec<Observation>> {
    exact_object(input, &["program", "observations", "options"], "evaluation input")?;
    let entries = input["observations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("evaluation observations are not an array"))?;
    entries
        .iter()
        .map(|entry| {
            exact_object(entry, &["status", "stdout", "stderr"], "evaluation observation")?;
            let status = entry["status"]
                .as_u64()
                .filter(|status| *status < 256)
                .ok_or_else(|| BridgeError::new("evaluation observation status is outside 0..255"))?;
            Ok(Observation {
                status: status as u8,
                stdout: require_bytes(&entry["stdout"], "evaluation stdout observation")?,
                stderr: require_bytes(&entry["stderr"], "evaluation stderr observation")?,
            })
        })
        .collect()
}

fn decode_options(input: &Value) -> BridgeResult<Options> {
    exact_object(&input["options"], &["errexit", "pipefail"], "evaluation options")?;
    Ok(Options {
        errexit: input["options"]["errexit"]
            .as_bool()
            .ok_or_else(|| BridgeError::new("evaluation errexit option is not Boolean"))?,
        pipefail: input["options"]["pipefail"]
            .as_bool()
            .ok_or_else(|| BridgeError::new("evaluation pipefail option is not Boolean"))?,
    })
}

fn parse_source(source: &[u8]) -> BridgeResult<Program> {
    Parser::new_bytes(source)
        .parse()
        .map_err(|error| BridgeError::new(format!("evaluation source did not parse: {error}")))
}

fn make_input(source: &[u8], observations: &[Observation], options: &Options) -> BridgeResult<Value> {
    let program = parse_source(source)?;
    Ok(json!({
        "program": program_json(&program)?,
        "observations": observations.iter().map(observation_json).collect::<Vec<_>>(),
        "options": {"errexit": options.errexit, "pipefail": options.pipefail},
    }))
}

fn observation_json(observation: &Observation) -> Value {
    json!({
        "status": observation.status,
        "stdout": observation.stdout.clone(),
        "stderr": observation.stderr.clone(),
    })
}

fn program_json(program: &Program) -> BridgeResult<Value> {
    let commands = program
        .commands
        .iter()
        .map(command_json)
        .collect::<BridgeResult<Vec<_>>>()?;
    let first = commands
        .first()
        .cloned()
        .ok_or_else(|| BridgeError::new("evaluation program is empty"))?;
    Ok(commands.into_iter().skip(1).fold(first, |left, right| {
        json!({"tag": "sequence", "left": left, "right": right})
    }))
}

fn command_json(command: &Command) -> BridgeResult<Value> {
    match command {
        Command::Simple { assigns, args, redirs, .. } => {
            if !redirs.is_empty() {
                return Err(BridgeError::new("evaluation source contains a redirection"));
            }
            if assigns.is_empty() && args.is_empty() {
                return Err(BridgeError::new("evaluation source has an empty simple command"));
            }
            let assignments = assigns
                .iter()
                .map(|assignment| {
                    if !is_shell_name(&assignment.name) {
                        return Err(BridgeError::new("evaluation assignment name is invalid"));
                    }
                    Ok(json!([
                        assignment.name,
                        literal_assignment_word(&assignment.value)?
                    ]))
                })
                .collect::<BridgeResult<Vec<_>>>()?;
            let mut assignment_names = HashSet::new();
            if assigns
                .iter()
                .any(|assignment| !assignment_names.insert(assignment.name.as_str()))
            {
                return Err(BridgeError::new(
                    "evaluation simple command has duplicate assignments",
                ));
            }
            let argv = args
                .iter()
                .map(literal_word)
                .collect::<BridgeResult<Vec<_>>>()?;
            let observation = if argv.is_empty() {
                0
            } else {
                callback_index(&argv[0])?
            };
            Ok(json!({
                "tag": "simple",
                "assignments": assignments,
                "argv": argv,
                "observation": observation,
            }))
        }
        Command::Sequence(left, right) => Ok(json!({
            "tag": "sequence",
            "left": command_json(left)?,
            "right": command_json(right)?,
        })),
        Command::And(left, right) => Ok(json!({
            "tag": "and",
            "left": command_json(left)?,
            "right": command_json(right)?,
        })),
        Command::Or(left, right) => Ok(json!({
            "tag": "or",
            "left": command_json(left)?,
            "right": command_json(right)?,
        })),
        Command::Not(body) => Ok(json!({"tag": "not", "body": command_json(body)?})),
        Command::Subshell { body, redirs, .. } => {
            if !redirs.is_empty() {
                return Err(BridgeError::new("evaluation subshell contains a redirection"));
            }
            Ok(json!({"tag": "subshell", "body": command_json(body)?}))
        }
        Command::Pipeline { commands, bang, .. } if commands.len() == 1 && *bang => {
            Ok(json!({"tag": "not", "body": command_json(&commands[0])?}))
        }
        Command::Pipeline { commands, bang, .. } => Ok(json!({
            "tag": "pipeline",
            "commands": commands.iter().map(command_json).collect::<BridgeResult<Vec<_>>>()?,
            "bang": bang,
        })),
        _ => Err(BridgeError::new(
            "evaluation source uses syntax outside the admitted command set",
        )),
    }
}

fn literal_word(word: &Word) -> BridgeResult<Vec<u8>> {
    let mut value = String::new();
    append_literal_parts(&word.parts, &mut value, false, true)?;
    let value = remove_glob_escapes(&value);
    Ok(ShellBytes::from_str_lossless(&value).into_vec())
}

fn literal_assignment_word(word: &Word) -> BridgeResult<Vec<u8>> {
    let mut value = String::new();
    append_literal_parts(&word.parts, &mut value, false, false)?;
    if value.contains('\u{FFFE}') {
        return Err(BridgeError::new(
            "evaluation assignment contains an escaped literal",
        ));
    }
    Ok(ShellBytes::from_str_lossless(&value).into_vec())
}

fn append_literal_parts(
    parts: &[WordPart],
    output: &mut String,
    quoted: bool,
    reject_globs: bool,
) -> BridgeResult<()> {
    for part in parts {
        match part {
            WordPart::Literal(bytes) => {
                let text = bytes.to_shell_string();
                if text.contains('\u{FFFE}') && (quoted || !reject_globs || text.ends_with('\u{FFFE}')) {
                    return Err(BridgeError::new(
                        "evaluation source contains a reserved escape marker",
                    ));
                }
                if reject_globs && !quoted && has_unescaped_glob(&text) {
                    return Err(BridgeError::new(
                        "evaluation source contains unquoted glob syntax",
                    ));
                }
                output.push_str(&text);
            }
            WordPart::SingleQuoted(bytes) => {
                let text = bytes.to_shell_string();
                if text.contains('\u{FFFE}') {
                    return Err(BridgeError::new(
                        "evaluation source contains a reserved escape marker",
                    ));
                }
                output.push_str(&text);
            }
            WordPart::DoubleQuoted(parts) => {
                append_literal_parts(parts, output, true, reject_globs)?;
            }
            _ => {
                return Err(BridgeError::new(
                    "evaluation source contains a word expansion",
                ));
            }
        }
    }
    Ok(())
}

fn has_unescaped_glob(value: &str) -> bool {
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            escaped = false;
        } else if character == '\u{FFFE}' {
            escaped = true;
        } else if matches!(character, '*' | '?' | '[' | ']') {
            return true;
        }
    }
    false
}

fn callback_index(name: &[u8]) -> BridgeResult<usize> {
    let name = std::str::from_utf8(name)
        .map_err(|_| BridgeError::new("evaluation command name is not UTF-8"))?;
    let suffix = name
        .strip_prefix(CALLBACK_PREFIX)
        .ok_or_else(|| BridgeError::new("evaluation command is not an observation callback"))?;
    let index = suffix
        .parse::<usize>()
        .map_err(|_| BridgeError::new("evaluation callback index is malformed"))?;
    if callback_name(index) != name {
        return Err(BridgeError::new("evaluation callback name is not canonical"));
    }
    Ok(index)
}

fn collect_observation_indices(program: &Value, indices: &mut Vec<usize>) -> BridgeResult<()> {
    match program["tag"].as_str() {
        Some("simple") => {
            if program["argv"].as_array().is_none_or(|argv| argv.is_empty()) {
                return Ok(());
            }
            let index = program["observation"]
                .as_u64()
                .ok_or_else(|| BridgeError::new("evaluation leaf index is malformed"))?;
            indices.push(index as usize);
        }
        Some("sequence" | "and" | "or") => {
            collect_observation_indices(&program["left"], indices)?;
            collect_observation_indices(&program["right"], indices)?;
        }
        Some("not" | "subshell") => collect_observation_indices(&program["body"], indices)?,
        Some("pipeline") => {
            let commands = program["commands"]
                .as_array()
                .ok_or_else(|| BridgeError::new("evaluation pipeline commands are malformed"))?;
            for command in commands {
                collect_observation_indices(command, indices)?;
            }
        }
        _ => return Err(BridgeError::new("evaluation expression tag is malformed")),
    }
    Ok(())
}

fn expression_depth(program: &Value) -> usize {
    match program["tag"].as_str() {
        Some("sequence" | "and" | "or") => {
            1 + expression_depth(&program["left"]).max(expression_depth(&program["right"]))
        }
        Some("not" | "subshell") => 1 + expression_depth(&program["body"]),
        Some("pipeline") => 1 + program["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .map(expression_depth)
            .max()
            .unwrap_or(0),
        _ => 1,
    }
}

fn validate_pipeline_observations(
    program: &Value,
    observations: &[Observation],
) -> BridgeResult<()> {
    match program["tag"].as_str() {
        Some("pipeline") => {
            let commands = program["commands"]
                .as_array()
                .ok_or_else(|| BridgeError::new("evaluation pipeline commands are malformed"))?;
            if commands.len() < 2 {
                return Err(BridgeError::new("evaluation pipeline must have at least two stages"));
            }
            for (index, command) in commands.iter().enumerate() {
                if command["tag"] != "simple"
                    || command["argv"].as_array().is_none_or(|argv| argv.is_empty())
                {
                    return Err(BridgeError::new(
                        "evaluation pipeline stages must be direct external commands",
                    ));
                }
                let observation_index = command["observation"]
                    .as_u64()
                    .ok_or_else(|| BridgeError::new("evaluation pipeline leaf index is malformed"))?
                    as usize;
                let observation = observations
                    .get(observation_index)
                    .ok_or_else(|| BridgeError::new("evaluation pipeline leaf has no observation"))?;
                if index + 1 < commands.len()
                    && (!observation.stdout.is_empty() || !observation.stderr.is_empty())
                {
                    return Err(BridgeError::new(
                        "nonfinal pipeline stages must have empty stdout and stderr",
                    ));
                }
                if index + 1 == commands.len() && !observation.stderr.is_empty() {
                    return Err(BridgeError::new(
                        "the final pipeline stage must have empty stderr",
                    ));
                }
            }
        }
        Some("sequence" | "and" | "or") => {
            validate_pipeline_observations(&program["left"], observations)?;
            validate_pipeline_observations(&program["right"], observations)?;
        }
        Some("not" | "subshell") => {
            validate_pipeline_observations(&program["body"], observations)?;
        }
        Some("simple") => {}
        _ => return Err(BridgeError::new("evaluation expression tag is malformed")),
    }
    Ok(())
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    validate_case(request)?;
    let initial_state = decode_state(&request.state)?;
    let observations = decode_observations(&request.input)?;
    let options = decode_options(&request.input)?;
    let parsed = parse_source(&request.source)?;
    let scratch = Scratch::new()?;
    let result = actual_in_scratch(
        &parsed,
        &initial_state,
        &observations,
        &options,
        &scratch,
    );
    match (result, scratch.remove()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(BridgeError::new(format!(
            "{error}; evaluation scratch cleanup failed: {cleanup}"
        ))),
    }
}

fn actual_in_scratch(
    parsed: &Program,
    initial_state: &InitialState,
    observations: &[Observation],
    options: &Options,
    scratch: &Scratch,
) -> BridgeResult<Value> {
    let cwd = PathBuf::from(OsString::from_vec(initial_state.cwd.clone()));
    let mut shell = Shell::builder()
        .cwd(cwd)
        .env_clear()
        .errexit(options.errexit)
        .pipefail(options.pipefail)
        .build();
    for (name, value) in &initial_state.variables {
        shell
            .set_var_bytes(name, ShellBytes::from_vec(value.clone()))
            .map_err(BridgeError::new)?;
    }
    for name in &initial_state.exported {
        shell.vars_mut().export(name);
    }
    let mut arguments = vec![ShellBytes::from("ish-evaluation")];
    arguments.extend(initial_state.positional.iter().cloned().map(ShellBytes::from_vec));
    shell.set_args_bytes(&arguments);
    if initial_state.last_status != 0 {
        let status = initial_state.last_status;
        shell.set_external_handler(Box::new(move |_, _| {
            Ok(ExitStatus::from(status as i32))
        }));
        let prime = Parser::new_bytes(PRIME_COMMAND)
            .parse()
            .map_err(|error| BridgeError::new(format!("cannot parse initial-status command: {error}")))?;
        shell.run_program(&prime);
        if shell.exit_status().code() != status as i32 {
            return Err(BridgeError::new("cannot prime the production shell status"));
        }
    }
    let callback_dir = scratch.path.clone();
    let callback_observations = observations.to_vec();
    shell.set_external_handler(Box::new(move |argv, environment| {
        let command = argv
            .first()
            .ok_or_else(|| ShellError::CommandNotFound(String::new()))?;
        let index = callback_index(command.as_bytes())
            .map_err(|error| ShellError::CommandNotFound(error.to_string()))?;
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(callback_dir.join("invocation-order"))
            .and_then(|mut file| file.write_all(format!("{index}\n").as_bytes()))
            .map_err(ShellError::Io)?;
        let observation = callback_observations
            .get(index)
            .ok_or_else(|| ShellError::CommandNotFound(command.to_shell_string()))?;
        let cwd = external_command_cwd()
            .ok_or_else(|| ShellError::Io(std::io::Error::other("callback has no shell cwd")))?;
        let mut environment: Vec<_> = environment
            .iter()
            .map(|(name, value)| {
                (
                    name.as_bytes().to_vec(),
                    value.as_bytes().to_vec(),
                )
            })
            .collect();
        environment.sort_by(|left, right| left.0.cmp(&right.0));
        let event = json!({
            "argv": argv.iter().map(|argument| argument.as_bytes().to_vec()).collect::<Vec<_>>(),
            "environment": environment,
            "cwd": cwd.as_os_str().as_bytes(),
            "stdout": observation.stdout,
            "stderr": observation.stderr,
        });
        let bytes = serde_json::to_vec(&event)
            .map_err(|error| ShellError::Io(std::io::Error::other(error.to_string())))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(callback_dir.join(event_name(index)))
            .map_err(ShellError::Io)?;
        file.write_all(&bytes).map_err(ShellError::Io)?;
        {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&observation.stdout).map_err(ShellError::Io)?;
            stdout.flush().map_err(ShellError::Io)?;
        }
        {
            let mut stderr = std::io::stderr().lock();
            stderr.write_all(&observation.stderr).map_err(ShellError::Io)?;
            stderr.flush().map_err(ShellError::Io)?;
        }
        Ok(ExitStatus::from(observation.status as i32))
    }));
    let capture = OutputCapture::new(scratch)?;
    let returned = shell.run_program(parsed).code();
    let shell_status = shell.exit_status().code();
    let (stdout, stderr) = capture.finish()?;
    if returned != shell_status {
        return Err(BridgeError::new(
            "production shell return status differs from its recorded status",
        ));
    }
    let status = u8::try_from(returned)
        .map_err(|_| BridgeError::new("production shell returned a status outside 0..255"))?;
    let program = program_json(parsed)?;
    let state = resulting_state(&program, initial_state, &shell, status)?;
    let trace = recorded_trace(scratch, &program, observations.len())?;
    Ok(json!({
        "tag": "success",
        "value": {
            "state": state,
            "status": status,
            "stdout": stdout,
            "stderr": stderr,
            "trace": trace,
        },
    }))
}

fn event_name(index: usize) -> String {
    format!("callback-{index:08}.json")
}

fn resulting_state(
    program: &Value,
    initial: &InitialState,
    shell: &Shell,
    status: u8,
) -> BridgeResult<Value> {
    let mut variable_names: BTreeSet<String> =
        initial.variables.iter().map(|(name, _)| name.clone()).collect();
    collect_persistent_assignment_names(program, &mut variable_names)?;
    let variables = variable_names
        .into_iter()
        .filter_map(|name| {
            shell
                .get_var_bytes(&name)
                .map(|value| json!([name, value.as_bytes()]))
        })
        .collect::<Vec<_>>();
    let mut exported_names = initial.exported.clone();
    exported_names.sort();
    let exported = exported_names
        .into_iter()
        .map(|name| {
            let value = shell
                .get_var_bytes(&name)
                .ok_or_else(|| BridgeError::new("exported evaluation variable became unset"))?;
            Ok(json!([name, value.as_bytes()]))
        })
        .collect::<BridgeResult<Vec<_>>>()?;
    Ok(json!({
        "variables": variables,
        "exported": exported,
        "inherited": [],
        "cwd": initial.cwd,
        "positional": initial.positional,
        "last_status": status,
    }))
}

fn collect_persistent_assignment_names(
    program: &Value,
    names: &mut BTreeSet<String>,
) -> BridgeResult<()> {
    match program["tag"].as_str() {
        Some("simple") => {
            if program["argv"].as_array().is_some_and(|argv| argv.is_empty()) {
                let assignments = program["assignments"]
                    .as_array()
                    .ok_or_else(|| BridgeError::new("evaluation assignments are malformed"))?;
                for assignment in assignments {
                    let name = assignment[0]
                        .as_str()
                        .ok_or_else(|| BridgeError::new("evaluation assignment name is malformed"))?;
                    names.insert(name.to_string());
                }
            }
        }
        Some("sequence" | "and" | "or") => {
            collect_persistent_assignment_names(&program["left"], names)?;
            collect_persistent_assignment_names(&program["right"], names)?;
        }
        Some("not" | "subshell") => {
            collect_persistent_assignment_names(&program["body"], names)?;
        }
        Some("pipeline") => {}
        _ => return Err(BridgeError::new("evaluation expression tag is malformed")),
    }
    Ok(())
}

fn pipeline_observation_groups(program: &Value, groups: &mut Vec<Vec<usize>>) -> BridgeResult<()> {
    match program["tag"].as_str() {
        Some("pipeline") => {
            let commands = program["commands"].as_array()
                .ok_or_else(|| BridgeError::new("evaluation pipeline commands are malformed"))?;
            let indices = commands.iter().map(|command| {
                command["observation"].as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| BridgeError::new("evaluation pipeline observation is malformed"))
            }).collect::<BridgeResult<Vec<_>>>()?;
            groups.push(indices);
        }
        Some("sequence" | "and" | "or") => {
            pipeline_observation_groups(&program["left"], groups)?;
            pipeline_observation_groups(&program["right"], groups)?;
        }
        Some("not" | "subshell") => pipeline_observation_groups(&program["body"], groups)?,
        Some("simple") => {}
        _ => return Err(BridgeError::new("evaluation expression tag is malformed")),
    }
    Ok(())
}

fn project_invocation_order(program: &Value, mut order: Vec<usize>) -> BridgeResult<Vec<usize>> {
    let mut pipelines = Vec::new();
    pipeline_observation_groups(program, &mut pipelines)?;
    let mut start = 0;
    while start < order.len() {
        let Some(pipeline) = pipelines.iter().find(|indices| indices.contains(&order[start])) else {
            start += 1;
            continue;
        };
        let end = start + order[start..].iter().take_while(|index| pipeline.contains(index)).count();
        order[start..end].sort_by_key(|index| pipeline.iter().position(|candidate| candidate == index));
        start = end;
    }
    Ok(order)
}

fn recorded_trace(scratch: &Scratch, program: &Value, observation_count: usize) -> BridgeResult<Vec<Value>> {
    let order = match fs::read_to_string(scratch.path.join("invocation-order")) {
        Ok(order) => order,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(BridgeError::new(format!("cannot read evaluation invocation order: {error}"))),
    };
    let mut seen = HashSet::new();
    let order = order.lines().map(|line| {
        let index = line.parse::<usize>()
            .map_err(|error| BridgeError::new(format!("invalid evaluation invocation index: {error}")))?;
        if index >= observation_count || !seen.insert(index) {
            return Err(BridgeError::new("evaluation invocation index is duplicated or out of range"));
        }
        Ok(index)
    }).collect::<BridgeResult<Vec<_>>>()?;
    let order = project_invocation_order(program, order)?;
    let mut trace = Vec::new();
    for index in order {
        let path = scratch.path.join(event_name(index));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(BridgeError::new(format!(
                    "cannot read evaluation callback event {}: {error}",
                    path.display()
                )));
            }
        };
        let event: Value = serde_json::from_slice(&bytes)
            .map_err(|error| BridgeError::new(format!("evaluation callback event is malformed: {error}")))?;
        trace.push(json!({
            "tag": "invoke",
            "argv": event["argv"],
            "environment": event["environment"],
            "cwd": event["cwd"],
        }));
        if !event["stdout"].as_array().is_some_and(Vec::is_empty) {
            trace.push(json!({"tag": "write", "fd": 1, "bytes": event["stdout"]}));
        }
        if !event["stderr"].as_array().is_some_and(Vec::is_empty) {
            trace.push(json!({"tag": "write", "fd": 2, "bytes": event["stderr"]}));
        }
    }
    Ok(trace)
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    if outcome["tag"] != "success" {
        return Err(BridgeError::new("evaluation negative control needs success"));
    }
    let status = outcome["value"]["status"]
        .as_u64()
        .filter(|status| *status < 256)
        .ok_or_else(|| BridgeError::new("evaluation outcome status is malformed"))?;
    let mut altered = outcome.clone();
    altered["value"]["status"] = json!(if status == 0 { 1 } else { 0 });
    altered["value"]["state"]["last_status"] = altered["value"]["status"].clone();
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    validate_case(request)?;
    let observations = decode_observations(&request.input)?;
    let options = decode_options(&request.input)?;
    let mut candidates = Vec::new();
    if let Some(observation) = observations.first() {
        let mut reduced_observations = Vec::new();
        let source = external_source(
            &mut reduced_observations,
            observation.status,
            observation.stdout.clone(),
            observation.stderr.clone(),
            &[],
            &[b"x".to_vec()],
        );
        let mut candidate = request.clone();
        candidate.source = source;
        candidate.input = make_input(&candidate.source, &reduced_observations, &options)?;
        if validate_case(&candidate).is_ok() {
            candidates.push(candidate);
        }
    }
    let initial = decode_state(&request.state)?;
    if let Some((name, _)) = initial.variables.first() {
        let mut candidate = request.clone();
        candidate.source = assignment_source(name, b"x");
        candidate.input = make_input(&candidate.source, &[], &options)?;
        if validate_case(&candidate).is_ok()
            && !candidates.iter().any(|existing| existing.input == candidate.input)
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

impl Scratch {
    fn new() -> BridgeResult<Self> {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work/tmp"));
        fs::create_dir_all(&base).map_err(|error| {
            BridgeError::new(format!("cannot create evaluation scratch base: {error}"))
        })?;
        for _ in 0..32 {
            let sequence = NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "ish-evaluation-{}-{sequence:016x}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, removed: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(BridgeError::new(format!(
                        "cannot create evaluation sandbox: {error}"
                    )));
                }
            }
        }
        Err(BridgeError::new(
            "cannot allocate a unique evaluation sandbox",
        ))
    }

    fn remove(mut self) -> BridgeResult<()> {
        fs::remove_dir_all(&self.path).map_err(|error| {
            BridgeError::new(format!("cannot remove evaluation sandbox: {error}"))
        })?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.removed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl OutputCapture {
    fn new(scratch: &Scratch) -> BridgeResult<Self> {
        std::io::stdout()
            .flush()
            .map_err(|error| BridgeError::new(format!("cannot flush stdout before capture: {error}")))?;
        std::io::stderr()
            .flush()
            .map_err(|error| BridgeError::new(format!("cannot flush stderr before capture: {error}")))?;
        let saved_stdout = rustix::io::dup(std::io::stdout())
            .map_err(|error| BridgeError::new(format!("cannot duplicate stdout: {error}")))?;
        let saved_stderr = rustix::io::dup(std::io::stderr())
            .map_err(|error| BridgeError::new(format!("cannot duplicate stderr: {error}")))?;
        let stdout = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(scratch.path.join("stdout.bin"))
            .map_err(|error| BridgeError::new(format!("cannot capture stdout: {error}")))?;
        let stderr = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(scratch.path.join("stderr.bin"))
            .map_err(|error| BridgeError::new(format!("cannot capture stderr: {error}")))?;
        replace_fd(&stdout, 1)
            .map_err(|error| BridgeError::new(format!("cannot redirect stdout: {error}")))?;
        if let Err(error) = replace_fd(&stderr, 2) {
            let _ = replace_fd(&saved_stdout, 1);
            return Err(BridgeError::new(format!("cannot redirect stderr: {error}")));
        }
        Ok(Self {
            stdout,
            stderr,
            saved_stdout,
            saved_stderr,
            restored: false,
        })
    }

    fn finish(mut self) -> BridgeResult<(Vec<u8>, Vec<u8>)> {
        std::io::stdout()
            .flush()
            .map_err(|error| BridgeError::new(format!("cannot flush captured stdout: {error}")))?;
        std::io::stderr()
            .flush()
            .map_err(|error| BridgeError::new(format!("cannot flush captured stderr: {error}")))?;
        let stdout_restore = replace_fd(&self.saved_stdout, 1);
        let stderr_restore = replace_fd(&self.saved_stderr, 2);
        match (stdout_restore, stderr_restore) {
            (Ok(()), Ok(())) => self.restored = true,
            (Err(stdout), Ok(())) => {
                return Err(BridgeError::new(format!("cannot restore stdout: {stdout}")));
            }
            (Ok(()), Err(stderr)) => {
                return Err(BridgeError::new(format!("cannot restore stderr: {stderr}")));
            }
            (Err(stdout), Err(stderr)) => {
                return Err(BridgeError::new(format!(
                    "cannot restore stdout: {stdout}; cannot restore stderr: {stderr}"
                )));
            }
        }
        self.stdout
            .seek(SeekFrom::Start(0))
            .map_err(|error| BridgeError::new(format!("cannot rewind captured stdout: {error}")))?;
        self.stderr
            .seek(SeekFrom::Start(0))
            .map_err(|error| BridgeError::new(format!("cannot rewind captured stderr: {error}")))?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.stdout
            .read_to_end(&mut stdout)
            .map_err(|error| BridgeError::new(format!("cannot read captured stdout: {error}")))?;
        self.stderr
            .read_to_end(&mut stderr)
            .map_err(|error| BridgeError::new(format!("cannot read captured stderr: {error}")))?;
        Ok((stdout, stderr))
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        if !self.restored {
            let _ = replace_fd(&self.saved_stdout, 1);
            let _ = replace_fd(&self.saved_stderr, 2);
        }
    }
}

fn replace_fd(source: &impl AsRawFd, target: i32) -> std::io::Result<()> {
    // SAFETY: the source fd is borrowed from a live File or OwnedFd, and target is stdout or stderr.
    let result = unsafe { libc::dup2(source.as_raw_fd(), target) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_callback_trace_preserves_observed_invocation_order() {
        let scratch = Scratch::new().expect("evaluation scratch");
        for index in [1, 0] {
            let event = json!({
                "argv": [callback_name(index).as_bytes()],
                "environment": [], "cwd": [47], "stdout": [], "stderr": [],
            });
            fs::write(scratch.path.join(event_name(index)), serde_json::to_vec(&event).unwrap()).unwrap();
        }
        fs::write(scratch.path.join("invocation-order"), b"1\n0\n").unwrap();
        let program = program_json(&parse_source(format!("{}; {}", callback_name(0), callback_name(1)).as_bytes()).unwrap()).unwrap();
        let trace = recorded_trace(&scratch, &program, 2).expect("recorded trace");
        assert_eq!(trace.iter().map(|event| event["argv"].clone()).collect::<Vec<_>>(), vec![
            json!([callback_name(1).as_bytes()]),
            json!([callback_name(0).as_bytes()]),
        ]);
        scratch.remove().expect("removed scratch");
    }

    #[test]
    fn pipeline_projection_preserves_order_between_sequential_groups() {
        let source = format!("{}; {} | {}; {}", callback_name(0), callback_name(1), callback_name(2), callback_name(3));
        let program = program_json(&parse_source(source.as_bytes()).unwrap()).unwrap();
        assert_eq!(project_invocation_order(&program, vec![0, 2, 1, 3]).unwrap(), [0, 1, 2, 3]);
        assert_eq!(project_invocation_order(&program, vec![3, 2, 1, 0]).unwrap(), [3, 1, 2, 0]);
        assert_eq!(project_invocation_order(&program, vec![0, 2, 3, 1]).unwrap(), [0, 2, 3, 1]);
    }
}
