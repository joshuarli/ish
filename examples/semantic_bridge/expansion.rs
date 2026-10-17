use crate::semantic_bridge::protocol::{Family, Request, empty_state, exact_object, require_bytes};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::ast::{Command, ParamOp, Program, Word, WordPart};
use epsh::error::{ExitStatus, ShellError};
use epsh::expand::ShellExpand;
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const LEGACY_FIXTURE_COUNT: usize = 53;

#[derive(Clone, Copy, PartialEq, Eq)]
enum IfsKind {
    Whitespace,
    Colon,
    RawByte,
    Unicode,
    Empty,
    WhitespaceColon,
    WhitespaceRawByte,
}

const IFS_KINDS: [IfsKind; 7] = [
    IfsKind::Whitespace,
    IfsKind::Colon,
    IfsKind::RawByte,
    IfsKind::Unicode,
    IfsKind::Empty,
    IfsKind::WhitespaceColon,
    IfsKind::WhitespaceRawByte,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValuePattern {
    Leading,
    Adjacent,
    Trailing,
    AllBoundaries,
    Empty,
}

const VALUE_PATTERNS: [ValuePattern; 5] = [
    ValuePattern::Leading,
    ValuePattern::Adjacent,
    ValuePattern::Trailing,
    ValuePattern::AllBoundaries,
    ValuePattern::Empty,
];

#[derive(Clone, Copy)]
enum SourceShape {
    UnquotedValue,
    QuotedValue,
    UnquotedAffixedValue,
    QuotedAffixedValue,
    QuotedAffixesWithUnquotedValue,
    UnquotedAtWithAffixes,
    QuotedAtWithAffixes,
    QuotedStar,
    UnquotedStar,
}

const SOURCE_SHAPES: [SourceShape; 9] = [
    SourceShape::UnquotedValue,
    SourceShape::QuotedValue,
    SourceShape::UnquotedAffixedValue,
    SourceShape::QuotedAffixedValue,
    SourceShape::QuotedAffixesWithUnquotedValue,
    SourceShape::UnquotedAtWithAffixes,
    SourceShape::QuotedAtWithAffixes,
    SourceShape::QuotedStar,
    SourceShape::UnquotedStar,
];

const COMBINATORIAL_PERIOD: usize = IFS_KINDS.len() * VALUE_PATTERNS.len() * SOURCE_SHAPES.len();

#[derive(Default)]
struct ObservationIds {
    substitution: usize,
    arithmetic: usize,
    all: std::collections::HashSet<String>,
    process: std::collections::HashSet<String>,
}

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    if index >= LEGACY_FIXTURE_COUNT {
        return generate_combinatorial(index, seed);
    }
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0xa0761d6478bd642f));
    let case = index % LEGACY_FIXTURE_COUNT;
    let source = match case {
        0 => "echo bridge-literal".to_string(),
        1 => "echo 'quoted *'".to_string(),
        2 => "echo mixed\"*\"quoted".to_string(),
        3 => "echo ${UNSET:-fallback}".to_string(),
        4 => "echo ${EMPTY-fallback}".to_string(),
        5 => "echo $VALUE".to_string(),
        6 => "echo \"$VALUE\"".to_string(),
        7 => "echo \"$@\"".to_string(),
        8 => "echo ~".to_string(),
        9 => "echo $(echo fixture)".to_string(),
        10 => "echo $((1 + 2))".to_string(),
        11 => "echo bridge-*.txt".to_string(),
        12 => "echo absent-*.txt".to_string(),
        13 => "echo bridge-\"*\"*.txt".to_string(),
        14 => "echo \"\"".to_string(),
        15 => "echo pre\"\"post".to_string(),
        16 => "echo \"${UNSET:-fallback}\"".to_string(),
        17 => "echo $EMPTY".to_string(),
        18 => "echo first \"second field\"".to_string(),
        19 => {
            let suffix = random.below(100_000);
            format!("echo bridge-{suffix:05}-*.txt")
        }
        20 => "echo $?".to_string(),
        21 => "echo $#".to_string(),
        22 => "echo \"$@\"".to_string(),
        23 => "echo $@".to_string(),
        24 => "echo \"$*\"".to_string(),
        25 => "echo $*".to_string(),
        26 => "echo $0".to_string(),
        27 => "echo $$".to_string(),
        28 => "echo $-".to_string(),
        29 => "echo $!".to_string(),
        30 => "echo \"$1-$2\"".to_string(),
        31 => "echo ${EMPTY:-fallback}".to_string(),
        32 => "echo 'snowman:☃'".to_string(),
        33 => "echo $VALUE".to_string(),
        34 => "echo $VALUE".to_string(),
        35 => "echo $VALUE".to_string(),
        36 => "echo \"$(echo alpha)$(echo beta)\"".to_string(),
        37 => "echo \"$((1))$((2))\"".to_string(),
        38 => "echo \"$(echo \"$(echo x)\")\"".to_string(),
        39 => format!("echo {tick}echo backtick{tick}", tick = '\x60'),
        40 => "echo $(false)$?".to_string(),
        41 => "echo \"$!\"".to_string(),
        42 => "echo \"$!\"".to_string(),
        43 => "echo \"$((x=4))$((1/0))\"".to_string(),
        44 => "echo \"$((x=4))$x\"".to_string(),
        45 => "echo \"$((x=3))$((x+2))$x\"".to_string(),
        46 => "echo ${bad!}".to_string(),
        47 => "echo \"$(( $EXPR ))\"".to_string(),
        48 => "echo \"$((1 + $(( $EXPR ))))\"".to_string(),
        49 => "echo \"$((1/0))$((2))\"".to_string(),
        50 => "echo 'literal:\u{fffe}'".to_string(),
        51 => "echo $(echo $((x=4)))$x".to_string(),
        _ => "echo $(cd /)".to_string(),
    };
    let mut state = empty_state();
    let value = match case {
        5 => vec![0xff],
        6 => "\u{e080}".as_bytes().to_vec(),
        33 => "leftλright".as_bytes().to_vec(),
        34 => vec![b'l', 0xff, b'r'],
        _ => b"alpha beta".to_vec(),
    };
    let ifs = match case {
        24 | 33 => "λ:".as_bytes().to_vec(),
        34 => vec![0xff],
        35 => Vec::new(),
        _ => vec![32, 9, 10],
    };
    let mut variables = vec![
        json!(["EMPTY", []]),
        json!(["HOME", [47, 98, 114, 105, 100, 103, 101, 45, 104, 111, 109, 101]]),
        json!(["IFS", ifs]),
        json!(["VALUE", value]),
    ];
    if matches!(case, 43..=45 | 51) {
        variables.push(json!(["x", [49]]));
    }
    if matches!(case, 47 | 48) {
        let value: &[u8] = if case == 47 { b"fresh=4" } else { b"nested=4" };
        variables.push(json!(["EXPR", value]));
    }
    state["variables"] = Value::Array(variables);
    state["last_status"] = json!(if case == 20 { 37 } else { 0 });
    state["positional"] = match case {
        7 | 21 | 22..=25 => json!([[111, 110, 101], [], [116, 104, 114, 101, 101]]),
        30 => json!([[97], [255]]),
        _ => json!([]),
    };
    let process_observations = match case {
        26 => vec![json!({"id": "parameter:0", "bytes": [115, 99, 114, 105, 112, 116, 255]})],
        27 => vec![json!({"id": "parameter:$", "bytes": std::process::id().to_string().bytes().collect::<Vec<_>>()})],
        28 => vec![json!({"id": "parameter:-", "bytes": []})],
        29 | 42 => vec![json!({"id": "parameter:!", "bytes": [52, 50]})],
        _ => Vec::new(),
    };
    let globs = match case {
        11 => json!([{
            "pattern": b"bridge-*.txt",
            "quoted": vec![false; b"bridge-*.txt".len()],
            "matches": [
                b"bridge-a.txt".to_vec(),
                "bridge-\u{e080}.txt".as_bytes().to_vec(),
                b"bridge-\xff.txt".to_vec(),
            ],
        }]),
        12 => json!([glob_json(
            b"absent-*.txt",
            vec![false; b"absent-*.txt".len()],
            &[],
        )]),
        13 => {
            let mut quoted = vec![false; b"bridge-**.txt".len()];
            quoted[b"bridge-".len()] = true;
            json!([glob_json(
                b"bridge-**.txt",
                quoted,
                &[b"bridge-*a.txt", b"bridge-*b.txt"],
            )])
        }
        19 => {
            let pattern = source
                .strip_prefix("echo ")
                .ok_or_else(|| BridgeError::new("generated expansion command has no echo prefix"))?;
            let pattern = pattern.as_bytes();
            let suffix = std::str::from_utf8(&pattern[7..12])
                .map_err(|_| BridgeError::new("generated glob suffix is not UTF-8"))?;
            let mut prefix = format!("bridge-{suffix}-").into_bytes();
            prefix.push(b'*');
            prefix.extend_from_slice(b".txt");
            let matched = format!("bridge-{suffix}-a.txt");
            json!([glob_json(
                &prefix,
                vec![false; prefix.len()],
                &[matched.as_bytes()],
            )])
        }
        _ => json!([]),
    };
    generated_request(index, source, state, process_observations, globs)
}

fn generate_combinatorial(index: usize, seed: u64) -> BridgeResult<Request> {
    let ordinal = (index - LEGACY_FIXTURE_COUNT) % COMBINATORIAL_PERIOD;
    let mut layout = Generator::new(seed ^ 0x9e37_79b9_7f4a_7c15);
    let ifs_kind = IFS_KINDS[(ordinal % IFS_KINDS.len()
        + layout.below(IFS_KINDS.len() as u64) as usize)
        % IFS_KINDS.len()];
    let value_pattern = VALUE_PATTERNS[(ordinal / IFS_KINDS.len() % VALUE_PATTERNS.len()
        + layout.below(VALUE_PATTERNS.len() as u64) as usize)
        % VALUE_PATTERNS.len()];
    let source_shape = SOURCE_SHAPES[(ordinal / (IFS_KINDS.len() * VALUE_PATTERNS.len())
        % SOURCE_SHAPES.len()
        + layout.below(SOURCE_SHAPES.len() as u64) as usize)
        % SOURCE_SHAPES.len()];
    let mut random = Generator::new(
        seed ^ (index as u64).wrapping_mul(0xd6e8_feb8_6659_fd93) ^ 0xa076_1d64_78bd_642f,
    );
    let affix = random.below(4) as usize;
    let (prefix, suffix) = [("pre", "post"), ("L", "R"), ("λ", "☃"), (":", ":")][affix];
    let ifs = generated_ifs(ifs_kind);
    let value = generated_value(ifs_kind, value_pattern);
    let source = match source_shape {
        SourceShape::UnquotedValue => "echo ${VALUE}".to_string(),
        SourceShape::QuotedValue => "echo \"${VALUE}\"".to_string(),
        SourceShape::UnquotedAffixedValue => format!("echo {prefix}${{VALUE}}{suffix}"),
        SourceShape::QuotedAffixedValue => format!("echo {prefix}\"${{VALUE}}\"{suffix}"),
        SourceShape::QuotedAffixesWithUnquotedValue => "echo 'pre'${VALUE}'post'".to_string(),
        SourceShape::UnquotedAtWithAffixes => format!("echo {prefix}$@{suffix}"),
        SourceShape::QuotedAtWithAffixes => format!("echo {prefix}\"$@\"{suffix}"),
        SourceShape::QuotedStar => "echo \"$*\"".to_string(),
        SourceShape::UnquotedStar => "echo $*".to_string(),
    };
    let positional = match value_pattern {
        ValuePattern::Leading => vec![
            generated_value(ifs_kind, ValuePattern::Adjacent),
            Vec::new(),
            generated_value(ifs_kind, ValuePattern::Trailing),
        ],
        ValuePattern::Adjacent => vec![
            generated_value(ifs_kind, ValuePattern::Trailing),
            Vec::new(),
            generated_value(ifs_kind, ValuePattern::AllBoundaries),
        ],
        ValuePattern::Trailing => vec![
            generated_value(ifs_kind, ValuePattern::AllBoundaries),
            Vec::new(),
            generated_value(ifs_kind, ValuePattern::Leading),
        ],
        ValuePattern::AllBoundaries => vec![
            generated_value(ifs_kind, ValuePattern::Leading),
            Vec::new(),
            generated_value(ifs_kind, ValuePattern::Adjacent),
        ],
        ValuePattern::Empty => vec![Vec::new(), Vec::new(), Vec::new()],
    };
    let mut state = empty_state();
    state["variables"] = json!([
        ["EMPTY", []],
        ["HOME", [47, 98, 114, 105, 100, 103, 101, 45, 104, 111, 109, 101]],
        ["IFS", ifs],
        ["VALUE", value],
    ]);
    state["positional"] = json!(positional);
    state["last_status"] = json!(0);
    generated_request(index, source, state, Vec::new(), json!([]))
}

fn generated_ifs(kind: IfsKind) -> Vec<u8> {
    match kind {
        IfsKind::Whitespace => vec![32, 9, 10],
        IfsKind::Colon => vec![b':'],
        IfsKind::RawByte => vec![0xff],
        IfsKind::Unicode => "λ".as_bytes().to_vec(),
        IfsKind::Empty => Vec::new(),
        IfsKind::WhitespaceColon => vec![32, 9, 10, b':'],
        IfsKind::WhitespaceRawByte => vec![32, 9, 10, 0xff],
    }
}

fn generated_value(kind: IfsKind, pattern: ValuePattern) -> Vec<u8> {
    if pattern == ValuePattern::Empty {
        return Vec::new();
    }
    let delimiter = match kind {
        IfsKind::Whitespace => b" ".as_slice(),
        IfsKind::Colon | IfsKind::Empty => b":".as_slice(),
        IfsKind::RawByte => b"\xff".as_slice(),
        IfsKind::Unicode => "λ".as_bytes(),
        IfsKind::WhitespaceColon => b":".as_slice(),
        IfsKind::WhitespaceRawByte => b"\xff".as_slice(),
    };
    let mixed_ifs = matches!(kind, IfsKind::WhitespaceColon | IfsKind::WhitespaceRawByte);
    let mut value = Vec::new();
    match pattern {
        ValuePattern::Leading => {
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.push(b'a');
        }
        ValuePattern::Adjacent => {
            value.push(b'a');
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.push(b'b');
        }
        ValuePattern::Trailing => {
            value.push(b'a');
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.push(b'b');
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
        }
        ValuePattern::AllBoundaries => {
            value.extend_from_slice(delimiter);
            value.push(b'a');
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
            value.push(b'b');
            value.extend_from_slice(delimiter);
            if mixed_ifs {
                value.extend_from_slice(b" \t");
            }
        }
        ValuePattern::Empty => return Vec::new(),
    }
    value
}

fn generated_request(
    index: usize,
    source: String,
    state: Value,
    process_observations: Vec<Value>,
    globs: Value,
) -> BridgeResult<Request> {
    let program = parse_program(&source)?;
    let words = command_words(&program)?;
    let observation_values = observation_map_from_slice(&process_observations)?;
    let process = process_values(&observation_values)?;
    let glob_observations = globs
        .as_array()
        .ok_or_else(|| BridgeError::new("generated expansion globs are not an array"))?;
    let effect_observations = with_glob_directory(glob_observations, |cwd| {
        observe_effects(&words, &state, &process, cwd)
    })?;
    let mut observations = process_observations;
    observations.extend(effect_observations);
    let mut ids = ObservationIds::default();
    let model_words = words
        .iter()
        .map(|word| word_json(word, &mut ids))
        .collect::<BridgeResult<Vec<_>>>()?;
    Ok(Request {
        id: format!("expansion-{index:05}"),
        family: Family::Expansion,
        source: source.into_bytes(),
        state,
        fuel: 128,
        input: json!({
            "words": model_words,
            "observations": observations,
            "globs": globs,
        }),
    })
}

fn glob_json(pattern: &[u8], quoted: Vec<bool>, matches: &[&[u8]]) -> Value {
    json!({
        "pattern": pattern,
        "quoted": quoted,
        "matches": matches,
    })
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    if request.source.len() > 256 {
        return Err(BridgeError::new("generated expansion source exceeds 256 bytes"));
    }
    exact_object(&request.input, &["words", "observations", "globs"], "expansion input")?;
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("expansion source is not UTF-8"))?;
    let program = parse_program(source)?;
    let words = command_words(&program)?;
    let mut ids = ObservationIds::default();
    let model_words = words
        .iter()
        .map(|word| word_json(word, &mut ids))
        .collect::<BridgeResult<Vec<_>>>()?;
    if request.input["words"] != json!(model_words) {
        return Err(BridgeError::new("expansion word oracle does not match parsed source"));
    }
    let observations = request.input["observations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion observations are not an array"))?;
    let mut observed_ids = std::collections::HashSet::new();
    for observation in observations {
        validate_observation(observation)?;
        let id = observation["id"]
            .as_str()
            .ok_or_else(|| BridgeError::new("expansion observation ID is not text"))?;
        if !observed_ids.insert(id.to_string()) {
            return Err(BridgeError::new("expansion observation ID is duplicated"));
        }
        if !ids.all.contains(id) {
            return Err(BridgeError::new(format!("unexpected expansion observation {id:?}")));
        }
    }
    let globs = request.input["globs"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion glob observations are not an array"))?;
    let mut keys = std::collections::HashSet::new();
    for glob in globs {
        exact_object(glob, &["pattern", "quoted", "matches"], "expansion glob observation")?;
        let pattern = require_bytes(&glob["pattern"], "expansion glob pattern")?;
        let quoted = glob["quoted"]
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion glob quote mask is not an array"))?;
        if pattern.len() != quoted.len() || quoted.iter().any(|value| !value.is_boolean()) {
            return Err(BridgeError::new("expansion glob quote mask has incorrect length or values"));
        }
        let matches = glob["matches"]
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion glob matches are not an array"))?;
        for matched in matches {
            let bytes = require_bytes(matched, "expansion glob match")?;
            safe_fixture_name(&bytes)?;
        }
        let key = (pattern, quoted.clone());
        if !keys.insert(key) {
            return Err(BridgeError::new("expansion glob observation is duplicated"));
        }
    }
    for word in words {
        validate_word_effects(&word, 0)?;
    }
    Ok(())
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    validate_case(request)?;
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("expansion source is not UTF-8"))?;
    let program = parse_program(source)?;
    let words = command_words(&program)?;
    let globs = request.input["globs"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion glob observations are not an array"))?;
    let mut ids = ObservationIds::default();
    for word in &words {
        let _ = word_json(word, &mut ids)?;
    }
    let observations = observation_map(request)?;
    let missing = ids
        .process
        .iter()
        .find(|id| !observations.contains_key(id.as_str()) && id.as_str() != "parameter:!");
    if let Some(id) = missing {
        return Ok(json!({
            "tag": "unsupported_domain",
            "reason": format!("missing process parameter observation: {id}"),
        }));
    }
    let process = process_values(&observations)?;
    let outcome = with_glob_directory(globs, |cwd| {
        let actual_observations = observe_effects(&words, &request.state, &process, cwd)?;
        let mut expected_effects = Vec::with_capacity(actual_observations.len());
        for actual_observation in &actual_observations {
            let id = actual_observation["id"]
                .as_str()
                .ok_or_else(|| BridgeError::new("production effect observation ID is not text"))?;
            let Some(expected) = observations.get(id) else {
                let kind = if id.starts_with("sub") { "substitution" } else { "arithmetic" };
                return Ok(json!({
                    "tag": "unsupported_domain",
                    "reason": format!("missing {kind} oracle: {id}"),
                }));
            };
            expected_effects.push((*expected).clone());
        }
        let initial_status = request.state["last_status"]
            .as_u64()
            .filter(|status| *status < 256)
            .ok_or_else(|| BridgeError::new("expansion initial status is outside 0..255"))? as u8;
        if !effect_observations_match(&expected_effects, &actual_observations, initial_status) {
            return Err(BridgeError::new("expansion effect observations differ from production execution"));
        }
        let mut shell = build_shell(&request.state, cwd, &process)?;
        let initial_bindings = shell.vars().bindings_bytes();
        let mut context = PrimitiveExpandContext::new(&mut shell, &process);
        let mut fields = Vec::new();
        for word in &words {
            let cwd = context.shell.cwd().to_path_buf();
            let expansion = epsh::expand::expand_word_to_fields(word, &mut context);
            if context.shell.cwd() != cwd {
                return Err(BridgeError::new("word expansion changed the caller cwd"));
            }
            match expansion {
                Ok(expanded) => fields.extend(
                    expanded
                        .into_iter()
                        .map(|field| ShellBytes::from_str_lossless(&field).into_vec()),
                ),
                Err(error) => {
                    let state = resulting_state(request, &initial_bindings, context.shell)?;
                    return semantic_error_outcome(error, state);
                }
            }
        }
        let state = resulting_state(request, &initial_bindings, context.shell)?;
        Ok(json!({
            "tag": "success",
            "value": {"fields": fields, "state": state, "trace": []},
        }))
    })?;
    Ok(outcome)
}

fn effect_observations_match(expected: &[Value], actual: &[Value], initial_status: u8) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut status = initial_status;
    for (expected, actual) in expected.iter().zip(actual) {
        if expected["id"] != actual["id"] || expected["bytes"] != actual["bytes"] {
            return false;
        }
        match expected.get("status") {
            Some(expected_status) if expected_status == &actual["status"] => {
                status = actual["status"].as_u64().unwrap_or_default() as u8;
            }
            Some(_) => return false,
            None => {
                if let Some(actual_status) = actual.get("status").and_then(Value::as_u64) {
                    if actual_status != u64::from(status) {
                        return false;
                    }
                    status = actual_status as u8;
                }
            }
        }
        if let Some(expected_bindings) = expected.get("bindings") {
            if expected_bindings != &actual["bindings"] {
                return false;
            }
        } else if actual["bindings"].as_array().is_none_or(|bindings| !bindings.is_empty()) {
            return false;
        }
        if let Some(expected_error) = expected.get("error") {
            if expected_error != actual.get("error").unwrap_or(&Value::Null) {
                return false;
            }
        } else if actual.get("error").is_some() {
            return false;
        }
    }
    true
}

#[derive(Default)]
struct ProcessValues {
    pid: Option<u32>,
    arg0: Option<ShellBytes>,
    flags: Option<String>,
    last_background_pid: Option<u32>,
}

struct PrimitiveExpandContext<'a> {
    shell: &'a mut epsh::eval::Shell,
    process: &'a ProcessValues,
}

impl<'a> PrimitiveExpandContext<'a> {
    fn new(shell: &'a mut epsh::eval::Shell, process: &'a ProcessValues) -> Self {
        Self { shell, process }
    }
}

impl ShellExpand for PrimitiveExpandContext<'_> {
    fn vars(&self) -> &epsh::var::Variables {
        self.shell.vars()
    }

    fn vars_mut(&mut self) -> &mut epsh::var::Variables {
        self.shell.vars_mut()
    }

    fn exit_status(&self) -> ExitStatus {
        self.shell.exit_status()
    }

    fn pid(&self) -> u32 {
        self.process.pid.unwrap_or_else(|| self.shell.pid())
    }

    fn cwd(&self) -> &Path {
        self.shell.cwd()
    }

    fn command_subst(&mut self, command: &Command) -> epsh::error::Result<String> {
        self.shell.command_subst(command)
    }

    fn shell_flags(&self) -> String {
        self.process.flags.clone().unwrap_or_default()
    }

    fn last_bg_pid(&self) -> Option<u32> {
        self.process.last_background_pid
    }

    fn nounset(&self) -> bool {
        self.shell.opts().nounset
    }

    fn noglob(&self) -> bool {
        self.shell.opts().noglob
    }
}

fn observation_map(request: &Request) -> BridgeResult<HashMap<&str, &Value>> {
    let entries = request.input["observations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion observations are not an array"))?;
    observation_map_from_slice(entries)
}

fn observation_map_from_slice(entries: &[Value]) -> BridgeResult<HashMap<&str, &Value>> {
    let mut observations = HashMap::new();
    for entry in entries {
        validate_observation(entry)?;
        let id = entry["id"]
            .as_str()
            .ok_or_else(|| BridgeError::new("expansion observation ID is not text"))?;
        if observations.insert(id, entry).is_some() {
            return Err(BridgeError::new("expansion observation ID is duplicated"));
        }
    }
    Ok(observations)
}

fn process_values(observations: &HashMap<&str, &Value>) -> BridgeResult<ProcessValues> {
    let mut process = ProcessValues::default();
    if let Some(value) = observations.get("parameter:$") {
        let bytes = require_bytes(&value["bytes"], "process PID observation")?;
        process.pid = Some(parse_observed_pid(&bytes, "process PID observation")?);
    }
    if let Some(value) = observations.get("parameter:0") {
        process.arg0 = Some(ShellBytes::from_vec(require_bytes(
            &value["bytes"],
            "process argument-zero observation",
        )?));
    }
    if let Some(value) = observations.get("parameter:-") {
        let bytes = require_bytes(&value["bytes"], "shell-flags observation")?;
        process.flags = Some(
            String::from_utf8(bytes)
                .map_err(|_| BridgeError::new("shell-flags observation is not UTF-8"))?,
        );
    }
    if let Some(value) = observations.get("parameter:!") {
        let bytes = require_bytes(&value["bytes"], "last-background-PID observation")?;
        process.last_background_pid = Some(parse_observed_pid(
            &bytes,
            "last-background-PID observation",
        )?);
    }
    Ok(process)
}

fn parse_observed_pid(bytes: &[u8], context: &str) -> BridgeResult<u32> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| BridgeError::new(format!("{context} is not UTF-8")))?;
    text.parse::<u32>()
        .map_err(|_| BridgeError::new(format!("{context} is not an unsigned 32-bit PID")))
}

fn validate_observation(value: &Value) -> BridgeResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| BridgeError::new("expansion observation is not an object"))?;
    if !object.contains_key("id")
        || !object.contains_key("bytes")
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "bytes" | "status" | "bindings" | "error"))
    {
        return Err(BridgeError::new("expansion observation has incorrect fields"));
    }
    if value["id"].as_str().is_none() {
        return Err(BridgeError::new("expansion observation ID is not text"));
    }
    let id = value["id"].as_str().unwrap_or_default();
    require_bytes(&value["bytes"], "expansion observation bytes")?;
    if let Some(status) = object.get("status") {
        status
            .as_u64()
            .filter(|status| *status < 256)
            .ok_or_else(|| BridgeError::new("expansion observation status is outside 0..255"))?;
    }
    if let Some(bindings) = object.get("bindings") {
        let bindings = bindings
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion observation bindings are not an array"))?;
        for binding in bindings {
            if binding.as_array().is_none_or(|pair| pair.len() != 2) {
                return Err(BridgeError::new("expansion observation binding is not a pair"));
            }
            let name = binding[0]
                .as_str()
                .ok_or_else(|| BridgeError::new("expansion observation binding name is not text"))?;
            if !ordinary_name(name) {
                return Err(BridgeError::new("expansion observation binding name is not ordinary"));
            }
            require_bytes(&binding[1], "expansion observation binding value")?;
        }
        if id.starts_with("sub") && !bindings.is_empty() {
            return Err(BridgeError::new(
                "command substitution observation changes parent bindings",
            ));
        }
    }
    if let Some(error) = object.get("error") {
        exact_object(error, &["kind", "message"], "expansion observation error")?;
        if !matches!(error["kind"].as_str(), Some("arithmetic" | "expansion"))
            || error["message"].as_str().is_none()
        {
            return Err(BridgeError::new("expansion observation error is malformed"));
        }
        if id.starts_with("arith") && error["kind"] != "arithmetic" {
            return Err(BridgeError::new("expansion observation error kind does not match its ID"));
        }
    }
    Ok(())
}

fn ordinary_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn build_shell(
    state: &Value,
    cwd: &Path,
    process: &ProcessValues,
) -> BridgeResult<epsh::eval::Shell> {
    let last_status = state["last_status"]
        .as_u64()
        .filter(|status| *status < 256)
        .ok_or_else(|| BridgeError::new("expansion initial status is outside 0..255"))? as u8;
    let mut shell = epsh::eval::Shell::builder()
        .cwd(cwd.to_path_buf())
        .env_clear()
        .external_handler(Box::new(move |_, _| {
            Ok(ExitStatus::from(i32::from(last_status)))
        }))
        .build();
    let variables = state["variables"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion variables are not an array"))?;
    for binding in variables {
        set_variable(&mut shell, binding)?;
    }
    let exported = state["exported"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion exported variables are not an array"))?;
    for binding in exported {
        if binding.as_array().is_none_or(|pair| pair.len() != 2) {
            return Err(BridgeError::new("expansion exported variable is not a pair"));
        }
        let name = binding[0]
            .as_str()
            .ok_or_else(|| BridgeError::new("expansion exported name is not text"))?;
        if shell.vars().get_bytes(name).is_none() {
            set_variable(&mut shell, binding)?;
        }
        shell.vars_mut().export(name);
    }
    let ifs = state_binding(state, "IFS")?.unwrap_or_else(|| vec![32, 9, 10]);
    shell
        .vars_mut()
        .set_bytes("IFS", ShellBytes::from_vec(ifs))
        .map_err(BridgeError::new)?;
    let positional = state["positional"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion positional state is not an array"))?
        .iter()
        .map(|bytes| require_bytes(bytes, "expansion positional argument").map(ShellBytes::from_vec))
        .collect::<BridgeResult<Vec<_>>>()?;
    let mut arguments = vec![process
        .arg0
        .clone()
        .unwrap_or_else(|| ShellBytes::from("semantic-bridge"))];
    arguments.extend(positional);
    shell.set_args_bytes(&arguments);
    if last_status != 0 {
        let primed_status = shell.run_script("semantic-bridge-status");
        if primed_status != i32::from(last_status) {
            return Err(BridgeError::new("cannot prime the production shell status"));
        }
    }
    Ok(shell)
}

fn state_binding(state: &Value, name: &str) -> BridgeResult<Option<Vec<u8>>> {
    for field in ["variables", "exported"] {
        let bindings = state[field]
            .as_array()
            .ok_or_else(|| BridgeError::new(format!("expansion {field} are not an array")))?;
        if let Some(binding) = bindings
            .iter()
            .find(|binding| binding[0].as_str() == Some(name))
        {
            return require_bytes(&binding[1], "expansion state binding").map(Some);
        }
    }
    Ok(None)
}

fn observe_effects(
    words: &[Word],
    state: &Value,
    process: &ProcessValues,
    cwd: &Path,
) -> BridgeResult<Vec<Value>> {
    let mut shell = build_shell(state, cwd, process)?;
    let mut ids = ObservationIds::default();
    let mut result = Vec::new();
    for word in words {
        observe_word_effects(word, &mut shell, process, &mut ids, &mut result)?;
        if result.last().is_some_and(|entry| entry.get("error").is_some()) {
            break;
        }
    }
    Ok(result)
}

fn observe_word_effects(
    word: &Word,
    shell: &mut epsh::eval::Shell,
    process: &ProcessValues,
    ids: &mut ObservationIds,
    result: &mut Vec<Value>,
) -> BridgeResult<()> {
    for part in &word.parts {
        match part {
            WordPart::DoubleQuoted(parts) => {
                let inner = Word { parts: parts.clone(), span: Default::default() };
                observe_word_effects(&inner, shell, process, ids, result)?;
            }
            WordPart::CmdSubst(command) | WordPart::Backtick(command) => {
                let id = format!("sub{}", ids.substitution);
                ids.substitution += 1;
                let cwd = shell.cwd().to_path_buf();
                let before = shell.vars().bindings_bytes();
                let (bytes, error) = match shell.command_subst(command) {
                    Ok(value) => (ShellBytes::from_str_lossless(&value).into_vec(), None),
                    Err(error) => (Vec::new(), Some(expansion_error_value(error)?)),
                };
                if shell.cwd() != cwd {
                    return Err(BridgeError::new("command substitution changed the caller cwd"));
                }
                let after = shell.vars().bindings_bytes();
                let bindings = changed_bindings(&before, &after);
                if !bindings.is_empty() {
                    return Err(BridgeError::new(
                        "command substitution changed parent bindings",
                    ));
                }
                let mut observation = json!({
                    "id": id,
                    "bytes": bytes,
                    "status": shell.exit_status().code().clamp(0, 255),
                    "bindings": bindings,
                });
                if let Some(error) = error {
                    observation["error"] = error;
                }
                result.push(observation);
            }
            WordPart::Arith(_) => {
                let id = format!("arith{}", ids.arithmetic);
                ids.arithmetic += 1;
                let before = shell.vars().bindings_bytes();
                let arithmetic_word = Word { parts: vec![part.clone()], span: Default::default() };
                let mut context = PrimitiveExpandContext::new(shell, process);
                match epsh::expand::expand_word_to_fields(&arithmetic_word, &mut context) {
                    Ok(fields) => {
                        if fields.len() != 1 {
                            return Err(BridgeError::new("arithmetic observation did not produce one field"));
                        }
                        let after = context.shell.vars().bindings_bytes();
                        result.push(json!({
                            "id": id,
                            "bytes": ShellBytes::from_str_lossless(&fields[0]).into_vec(),
                            "bindings": changed_bindings(&before, &after),
                        }));
                    }
                    Err(error) => {
                        let message = match &error {
                            ShellError::Runtime { msg, .. } => msg.clone(),
                            _ => error.to_string(),
                        };
                        let after = context.shell.vars().bindings_bytes();
                        result.push(json!({
                            "id": id,
                            "bytes": [],
                            "bindings": changed_bindings(&before, &after),
                            "error": {"kind": "arithmetic", "message": message},
                        }));
                        return Ok(());
                    }
                }
            }
            WordPart::Param(parameter) => {
                if let ParamOp::Default { word, .. } = &parameter.op {
                    let inner = Word { parts: word.clone(), span: Default::default() };
                    observe_word_effects(&inner, shell, process, ids, result)?;
                }
            }
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => {}
        }
        if result.last().is_some_and(|entry| entry.get("error").is_some()) {
            return Ok(());
        }
    }
    Ok(())
}

fn changed_bindings(
    before: &[(String, ShellBytes)],
    after: &[(String, ShellBytes)],
) -> Vec<Value> {
    after
        .iter()
        .filter_map(|(name, value)| {
            let previous = before
                .iter()
                .find(|(previous_name, _)| previous_name == name)
                .map(|(_, previous_value)| previous_value);
            (previous != Some(value)).then(|| json!([name, value.as_bytes()]))
        })
        .collect()
}

fn resulting_state(
    request: &Request,
    initial_bindings: &[(String, ShellBytes)],
    shell: &epsh::eval::Shell,
) -> BridgeResult<Value> {
    let mut state = request.state.clone();
    let current_bindings = shell.vars().bindings_bytes();
    let changes: Vec<_> = changed_bindings(initial_bindings, &current_bindings);
    let variables = state["variables"]
        .as_array_mut()
        .ok_or_else(|| BridgeError::new("expansion variables are not an array"))?;
    for change in &changes {
        let name = change[0]
            .as_str()
            .ok_or_else(|| BridgeError::new("expansion changed variable name is not text"))?;
        let value = require_bytes(&change[1], "expansion changed variable value")?;
        if let Some(binding) = variables
            .iter_mut()
            .find(|binding| binding[0].as_str() == Some(name))
        {
            binding[1] = json!(value);
        } else {
            variables.push(json!([name, value]));
        }
    }
    let exported = state["exported"]
        .as_array_mut()
        .ok_or_else(|| BridgeError::new("expansion exported variables are not an array"))?;
    for binding in exported {
        let name = binding[0]
            .as_str()
            .ok_or_else(|| BridgeError::new("expansion exported name is not text"))?;
        if let Some(change) = changes.iter().find(|change| change[0].as_str() == Some(name)) {
            binding[1] = change[1].clone();
        }
    }
    state["last_status"] = json!(shell.exit_status().code().clamp(0, 255));
    Ok(state)
}

fn semantic_error_outcome(error: ShellError, state: Value) -> BridgeResult<Value> {
    let kind = expansion_error_kind(&error).ok_or_else(|| {
        BridgeError::new(format!("production expansion error is outside semantic domain: {error}"))
    })?;
    let message = expansion_error_message(error)?;
    Ok(json!({
        "tag": "semantic_error",
        "error": {"kind": kind, "message": message},
        "state": state,
        "trace": [],
    }))
}

fn expansion_error_value(error: ShellError) -> BridgeResult<Value> {
    let kind = expansion_error_kind(&error).ok_or_else(|| {
        BridgeError::new(format!("command substitution error is outside semantic domain: {error}"))
    })?;
    let message = expansion_error_message(error)?;
    Ok(json!({"kind": kind, "message": message}))
}

fn expansion_error_kind(error: &ShellError) -> Option<&'static str> {
    match error {
        ShellError::Runtime { msg, .. } if msg.starts_with("arithmetic error:") => Some("arithmetic"),
        ShellError::Runtime { .. } | ShellError::CommandNotFound(_) => Some("expansion"),
        _ => None,
    }
}

fn expansion_error_message(error: ShellError) -> BridgeResult<String> {
    match error {
        ShellError::Runtime { msg, .. } => Ok(msg),
        ShellError::CommandNotFound(name) => Ok(format!("{name}: not found")),
        error => Err(BridgeError::new(format!(
            "production expansion failed outside semantic domain: {error}"
        ))),
    }
}

fn set_variable(shell: &mut epsh::eval::Shell, binding: &Value) -> BridgeResult<()> {
    if binding.as_array().is_none_or(|pair| pair.len() != 2) {
        return Err(BridgeError::new("expansion variable is not a pair"));
    }
    let name = binding[0]
        .as_str()
        .ok_or_else(|| BridgeError::new("expansion variable name is not text"))?;
    let value = require_bytes(&binding[1], "expansion variable")?;
    shell
        .vars_mut()
        .set_bytes(name, ShellBytes::from_vec(value))
        .map_err(BridgeError::new)
}

fn with_glob_directory<T>(
    globs: &[Value],
    run: impl FnOnce(&Path) -> BridgeResult<T>,
) -> BridgeResult<T> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work/tmp/semantic-bridge-expansion");
    fs::create_dir_all(&root)
        .map_err(|error| BridgeError::new(format!("cannot create expansion fixture root: {error}")))?;
    let directory = loop {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(BridgeError::new(format!("cannot create expansion fixture: {error}"))),
        }
    };
    let result = (|| {
        for glob in globs {
            let matches = glob["matches"]
                .as_array()
                .ok_or_else(|| BridgeError::new("expansion glob matches are not an array"))?;
            for matched in matches {
                let bytes = require_bytes(matched, "expansion glob match")?;
                let name = safe_fixture_name(&bytes)?;
                fs::write(directory.join(name), [])
                    .map_err(|error| BridgeError::new(format!("cannot create expansion match fixture: {error}")))?;
            }
        }
        run(&directory)
    })();
    let cleanup = fs::remove_dir_all(&directory)
        .map_err(|error| BridgeError::new(format!("cannot remove expansion fixture: {error}")));
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(result), Err(cleanup)) => Err(BridgeError::new(format!("{result}; {cleanup}"))),
    }
}

fn safe_fixture_name(bytes: &[u8]) -> BridgeResult<OsString> {
    let name = OsString::from_vec(bytes.to_vec());
    let mut components = Path::new(&name).components();
    let valid = matches!(components.next(), Some(Component::Normal(component)) if component.as_bytes() == bytes);
    if !valid || components.next().is_some() || bytes.is_empty() || bytes.contains(&0) {
        return Err(BridgeError::new("expansion fixture name is not one safe path component"));
    }
    Ok(name)
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    let mut altered = outcome.clone();
    if altered["tag"] != "success" {
        return Err(BridgeError::new("expansion negative control needs success"));
    }
    let fields = altered["value"]["fields"]
        .as_array_mut()
        .ok_or_else(|| BridgeError::new("expansion result fields are malformed"))?;
    if let Some(first) = fields.first_mut() {
        first
            .as_array_mut()
            .ok_or_else(|| BridgeError::new("expansion result field is not bytes"))?
            .push(json!(0));
    } else {
        fields.push(json!([0]));
    }
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    let mut candidates = Vec::new();
    let mut sources = HashSet::new();
    for index in [20, 41, 0, 1, 3, 11, 12, 13, 46] {
        let mut candidate = generate(index, 0)?;
        if candidate.source.len() >= request.source.len() || !sources.insert(candidate.source.clone()) {
            continue;
        }
        candidate.id = request.id.clone();
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn parse_program(source: &str) -> BridgeResult<Program> {
    Parser::new(source)
        .parse()
        .map_err(|error| BridgeError::new(format!("generated expansion source did not parse: {error}")))
}

fn command_words(program: &Program) -> BridgeResult<Vec<Word>> {
    let [Command::Simple { assigns, args, redirs, .. }] = program.commands.as_slice() else {
        return Err(BridgeError::new("expansion fixture must be one simple command"));
    };
    if !assigns.is_empty() || !redirs.is_empty() || args.is_empty() || static_word(&args[0]).as_deref() != Some(b"echo") {
        return Err(BridgeError::new("expansion fixture must be a plain echo command"));
    }
    for word in &args[1..] {
        validate_word_effects(word, 0)?;
    }
    Ok(args[1..].to_vec())
}

fn static_word(word: &Word) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    for part in &word.parts {
        match part {
            WordPart::Literal(bytes) | WordPart::SingleQuoted(bytes) => result.extend_from_slice(bytes.as_bytes()),
            WordPart::DoubleQuoted(parts) => {
                result.extend(static_parts(parts)?);
            }
            _ => return None,
        }
    }
    Some(result)
}

fn static_parts(parts: &[WordPart]) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    for part in parts {
        match part {
            WordPart::Literal(bytes) | WordPart::SingleQuoted(bytes) => result.extend_from_slice(bytes.as_bytes()),
            _ => return None,
        }
    }
    Some(result)
}

fn validate_word_effects(word: &Word, depth: usize) -> BridgeResult<()> {
    if depth > 8 {
        return Err(BridgeError::new("expansion fixture nesting exceeds eight levels"));
    }
    for part in &word.parts {
        match part {
            WordPart::DoubleQuoted(parts) => validate_parts_effects(parts, depth + 1)?,
            WordPart::CmdSubst(command) | WordPart::Backtick(command) => validate_substitution(command, depth + 1)?,
            WordPart::Arith(parts) => validate_parts_effects(parts, depth + 1)?,
            WordPart::Param(parameter) => match &parameter.op {
                ParamOp::Normal | ParamOp::BadSubst => {}
                ParamOp::Default { word, .. } => {
                    if static_parts(word).is_none() {
                        return Err(BridgeError::new("expansion fixture parameter default is not literal"));
                    }
                }
                _ => return Err(BridgeError::new("expansion fixture uses an unsupported parameter operator")),
            },
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => {}
        }
    }
    Ok(())
}

fn validate_parts_effects(parts: &[WordPart], depth: usize) -> BridgeResult<()> {
    for part in parts {
        let word = Word { parts: vec![part.clone()], span: Default::default() };
        validate_word_effects(&word, depth + 1)?;
    }
    Ok(())
}

fn validate_substitution(command: &Command, depth: usize) -> BridgeResult<()> {
    if depth > 8 {
        return Err(BridgeError::new("expansion fixture nesting exceeds eight levels"));
    }
    let Command::Simple { assigns, args, redirs, .. } = command else {
        return Err(BridgeError::new("expansion fixture substitution is not a simple command"));
    };
    if !assigns.is_empty() || !redirs.is_empty() || args.is_empty() {
        return Err(BridgeError::new("expansion fixture substitution has unsupported effects"));
    }
    match static_word(&args[0]).as_deref() {
        Some(b"false") if args.len() == 1 => Ok(()),
        Some(b"cd") if args.len() == 2 && static_word(&args[1]).as_deref() == Some(b"/") => Ok(()),
        Some(b"echo") => {
            for word in &args[1..] {
                validate_word_effects(word, depth + 1)?;
            }
            Ok(())
        }
        _ => Err(BridgeError::new("expansion fixture substitution command is outside the safe builtin grammar")),
    }
}

fn word_json(word: &Word, ids: &mut ObservationIds) -> BridgeResult<Value> {
    Ok(Value::Array(parts_json(&word.parts, false, ids)?))
}

fn parts_json(parts: &[WordPart], quoted: bool, ids: &mut ObservationIds) -> BridgeResult<Vec<Value>> {
    let mut result = Vec::new();
    for part in parts {
        match part {
            WordPart::Literal(bytes) => result.push(json!({
                "tag": if quoted { "quoted" } else { "literal" },
                "bytes": bytes.as_bytes(),
            })),
            WordPart::SingleQuoted(bytes) => result.push(json!({"tag": "quoted", "bytes": bytes.as_bytes()})),
            WordPart::DoubleQuoted(inner) if inner.is_empty() => {
                result.push(json!({"tag": "quoted", "bytes": []}));
            }
            WordPart::DoubleQuoted(inner) => result.extend(parts_json(inner, true, ids)?),
            WordPart::Param(parameter) => {
                let default = match &parameter.op {
                    ParamOp::Normal => Value::Null,
                    ParamOp::Default { colon, word } => json!({"colon": colon, "bytes": static_parts(word)
                        .ok_or_else(|| BridgeError::new("parameter default is not literal"))?}),
                    ParamOp::BadSubst => {
                        result.push(json!({"tag": "bad_substitution", "name": parameter.name}));
                        continue;
                    }
                    _ => return Err(BridgeError::new("unsupported expansion parameter operator")),
                };
                if matches!(parameter.name.as_str(), "$" | "0" | "-" | "!") {
                    let id = format!("parameter:{}", parameter.name);
                    ids.all.insert(id.clone());
                    ids.process.insert(id);
                }
                result.push(json!({
                    "tag": "parameter",
                    "name": parameter.name,
                    "quoted": quoted,
                    "default": default,
                }));
            }
            WordPart::Tilde(user) => result.push(json!({"tag": "tilde", "user": user.as_bytes()})),
            WordPart::CmdSubst(_) | WordPart::Backtick(_) => {
                let id = format!("sub{}", ids.substitution);
                ids.substitution += 1;
                register_effect(ids, &id);
                result.push(json!({"tag": "substitution", "id": id, "quoted": quoted}));
            }
            WordPart::Arith(_) => {
                let id = format!("arith{}", ids.arithmetic);
                ids.arithmetic += 1;
                register_effect(ids, &id);
                result.push(json!({"tag": "arithmetic", "id": id, "quoted": quoted}));
            }
        }
    }
    Ok(result)
}

fn register_effect(ids: &mut ObservationIds, id: &str) {
    ids.all.insert(id.to_string());
}

#[cfg(test)]
mod tests {
    use super::{
        actual, command_words, generate, parse_program, PrimitiveExpandContext, ProcessValues,
        COMBINATORIAL_PERIOD, LEGACY_FIXTURE_COUNT,
    };
    use epsh::error::ShellError;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn combinatorial_generation_covers_ifs_boundaries_and_word_contexts() {
        let requests: Vec<_> = (LEGACY_FIXTURE_COUNT..LEGACY_FIXTURE_COUNT + COMBINATORIAL_PERIOD)
            .map(|index| generate(index, 0x53ad_5eed).unwrap())
            .collect();
        let ifs_values: HashSet<Vec<u8>> = requests
            .iter()
            .map(|request| {
                request.state["variables"][2][1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect()
            })
            .collect();
        let values: HashSet<Vec<u8>> = requests
            .iter()
            .map(|request| {
                request.state["variables"][3][1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect()
            })
            .collect();
        let sources: HashSet<String> = requests
            .iter()
            .map(|request| String::from_utf8(request.source.clone()).unwrap())
            .collect();

        assert!(ifs_values.contains(b" \t\n".as_slice()));
        assert!(ifs_values.contains(b":".as_slice()));
        assert!(ifs_values.contains(&vec![0xff]));
        assert!(ifs_values.contains("λ".as_bytes()));
        assert!(ifs_values.contains(&Vec::new()));
        assert!(ifs_values.contains(b" \t\n:".as_slice()));
        assert!(ifs_values.contains(&vec![32, 9, 10, 0xff]));
        assert!(values.contains(b":a".as_slice()));
        assert!(values.contains(b"a::b".as_slice()));
        assert!(values.contains(b"a::b:".as_slice()));
        assert!(values.contains(b":a::b:".as_slice()));
        assert!(values.contains(&vec![0xff, b'a', 0xff, 0xff, b'b', 0xff]));
        assert!(values.contains("λaλλbλ".as_bytes()));
        assert!(values.contains(&Vec::new()));
        assert!(values.contains(b"a \t: \t: \tb".as_slice()));
        assert!(values.contains(b"a \t: \t: \tb: \t".as_slice()));
        assert!(values.contains(&vec![b'a', b' ', b'\t', 0xff, b' ', b'\t', 0xff, b' ', b'\t', b'b']));
        assert!(sources.contains("echo ${VALUE}"));
        assert!(sources.contains("echo \"${VALUE}\""));
        assert!(sources.iter().any(|source| {
            source.contains("${VALUE}")
                && !source.contains('"')
                && !source.contains('\'')
                && source != "echo ${VALUE}"
        }));
        assert!(sources
            .iter()
            .any(|source| source.contains("${VALUE}") && source.contains('\'')));
        assert!(sources.iter().any(|source| source.contains("$@") && source.contains('"')));
        assert!(sources.iter().any(|source| source.contains("$@") && !source.contains('"')));
        assert!(sources.contains("echo \"$*\""));
        assert!(sources.contains("echo $*"));
        assert!(requests.iter().any(|request| {
            request.state["variables"][2][1] == json!([58])
                && request.state["variables"][3][1] == json!([97, 58, 58, 98, 58])
        }));
        let combinations: HashSet<(usize, usize, usize)> = requests
            .iter()
            .map(|request| {
                let ifs: Vec<u8> = request.state["variables"][2][1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect();
                let value: Vec<u8> = request.state["variables"][3][1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect();
                let ifs_kind = match ifs.as_slice() {
                    b" \t\n" => 0,
                    b":" => 1,
                    [0xff] => 2,
                    [0xce, 0xbb] => 3,
                    [] => 4,
                    b" \t\n:" => 5,
                    [32, 9, 10, 0xff] => 6,
                    _ => panic!("unexpected generated IFS bytes"),
                };
                let pattern = value_pattern(ifs_kind, &value);
                let source = std::str::from_utf8(&request.source).unwrap();
                let shape = match source {
                    "echo ${VALUE}" => 0,
                    "echo \"${VALUE}\"" => 1,
                    "echo \"$*\"" => 7,
                    "echo $*" => 8,
                    _ if source.contains("${VALUE}") && source.contains('\'') => 4,
                    _ if source.contains("${VALUE}") && source.contains('"') => 3,
                    _ if source.contains("${VALUE}") => 2,
                    _ if source.contains("$@") && source.contains('"') => 6,
                    _ if source.contains("$@") => 5,
                    _ => panic!("unexpected generated source shape"),
                };
                (ifs_kind, pattern, shape)
            })
            .collect();
        assert_eq!(combinations.len(), COMBINATORIAL_PERIOD);
        for request in &requests {
            super::validate_case(request).unwrap();
        }

        let first = generate(LEGACY_FIXTURE_COUNT, 1).unwrap();
        let repeated = generate(LEGACY_FIXTURE_COUNT, 1).unwrap();
        let other_seed = generate(LEGACY_FIXTURE_COUNT, 2).unwrap();
        assert_eq!(first.to_json(), repeated.to_json());
        assert_ne!(first.to_json(), other_seed.to_json());
        assert_eq!(generate(52, 0).unwrap().source, b"echo $(cd /)");
    }

    fn value_pattern(ifs_kind: usize, value: &[u8]) -> usize {
        if value.is_empty() {
            return 4;
        }
        let delimiter: &[u8] = match ifs_kind {
            0 => b" ",
            1 | 4 | 5 => b":",
            2 => b"\xff",
            3 => "λ".as_bytes(),
            6 => b"\xff",
            _ => panic!("unexpected generated IFS class"),
        };
        let mixed_ifs = matches!(ifs_kind, 5 | 6);
        let adjacent = adjacent_delimiters(value, delimiter, mixed_ifs);
        let mut end = value.len();
        if mixed_ifs {
            while end > 0 && matches!(value[end - 1], b' ' | b'\t' | b'\n') {
                end -= 1;
            }
        }
        match (
            value.starts_with(delimiter),
            adjacent,
            value[..end].ends_with(delimiter),
        ) {
            (true, false, false) => 0,
            (false, true, false) => 1,
            (false, true, true) => 2,
            (true, true, true) => 3,
            _ => panic!("unexpected generated value shape"),
        }
    }

    fn adjacent_delimiters(value: &[u8], delimiter: &[u8], mixed_ifs: bool) -> bool {
        if !mixed_ifs {
            return value
                .windows(delimiter.len() * 2)
                .any(|pair| pair == [delimiter, delimiter].concat());
        }
        for start in 0..=value.len().saturating_sub(delimiter.len()) {
            if !value[start..].starts_with(delimiter) {
                continue;
            }
            let mut next = start + delimiter.len();
            while next < value.len() && matches!(value[next], b' ' | b'\t' | b'\n') {
                next += 1;
            }
            if value[next..].starts_with(delimiter) {
                return true;
            }
        }
        false
    }

    #[test]
    fn output_and_glob_fixtures_preserve_raw_bytes_and_unicode_scalars() {
        let invalid = actual(&generate(5, 0).unwrap()).unwrap();
        let literal_pua = actual(&generate(6, 0).unwrap()).unwrap();
        let literal_noncharacter = actual(&generate(50, 0).unwrap()).unwrap();
        let glob = actual(&generate(11, 0).unwrap()).unwrap();

        assert_eq!(invalid["value"]["fields"], json!([[255]]));
        assert_eq!(literal_pua["value"]["fields"], json!([[238, 130, 128]]));
        assert_eq!(
            literal_noncharacter["value"]["fields"],
            json!(["literal:\u{fffe}".as_bytes()])
        );
        assert_eq!(
            glob["value"]["fields"],
            json!([
                [98, 114, 105, 100, 103, 101, 45, 97, 46, 116, 120, 116],
                [98, 114, 105, 100, 103, 101, 45, 238, 130, 128, 46, 116, 120, 116],
                [98, 114, 105, 100, 103, 101, 45, 255, 46, 116, 120, 116],
            ])
        );
    }

    #[test]
    fn command_substitution_isolates_nested_assignments() {
        let request = generate(51, 0).unwrap();
        let outcome = actual(&request).unwrap();

        assert_eq!(request.input["observations"], json!([
            {"id": "sub0", "bytes": [52], "status": 0, "bindings": []}
        ]));
        assert_eq!(outcome["value"]["fields"], json!([[52, 49]]));
        assert_eq!(
            outcome["value"]["state"]["variables"].as_array().unwrap().last(),
            Some(&json!(["x", [49]]))
        );
    }

    #[test]
    fn command_substitution_isolates_cwd_changes() {
        let request = generate(52, 0).unwrap();
        let outcome = actual(&request).unwrap();

        assert_eq!(request.input["observations"], json!([
            {"id": "sub0", "bytes": [], "status": 0, "bindings": []}
        ]));
        assert_eq!(outcome["value"]["fields"], json!([]));
        assert_eq!(outcome["value"]["state"]["cwd"], request.state["cwd"]);
        assert_eq!(outcome["value"]["state"]["variables"], request.state["variables"]);
    }

    #[test]
    fn command_substitution_errors_escape_word_expansion() {
        let program = parse_program("echo $(echo interrupted)").unwrap();
        let words = command_words(&program).unwrap();
        let cancellation = Arc::new(AtomicBool::new(true));
        let mut shell = epsh::eval::Shell::new();
        shell.set_cancel_flag(cancellation);
        let process = ProcessValues::default();
        let mut context = PrimitiveExpandContext::new(&mut shell, &process);

        let error = epsh::expand::expand_word_to_fields(&words[0], &mut context).unwrap_err();

        assert!(matches!(error, ShellError::Cancelled));
    }

    #[test]
    fn effect_ids_are_unique_and_substitution_status_flows_to_later_parts() {
        let sibling_substitutions = generate(36, 0).unwrap();
        let sibling_ids: Vec<_> = sibling_substitutions.input["observations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|observation| observation["id"].as_str().unwrap())
            .collect();
        let sibling_output = actual(&sibling_substitutions).unwrap();
        let nested_substitution = actual(&generate(38, 0).unwrap()).unwrap();
        let backtick = actual(&generate(39, 0).unwrap()).unwrap();
        let arithmetic = generate(37, 0).unwrap();
        let arithmetic_ids: Vec<_> = arithmetic.input["observations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|observation| observation["id"].as_str().unwrap())
            .collect();
        let status = actual(&generate(40, 0).unwrap()).unwrap();

        assert_eq!(sibling_ids, ["sub0", "sub1"]);
        assert_eq!(sibling_output["value"]["fields"], json!([[97, 108, 112, 104, 97, 98, 101, 116, 97]]));
        assert_eq!(nested_substitution["value"]["fields"], json!([[120]]));
        assert_eq!(backtick["value"]["fields"], json!([[98, 97, 99, 107, 116, 105, 99, 107]]));
        assert_eq!(arithmetic_ids, ["arith0", "arith1"]);
        assert_eq!(status["value"]["fields"], json!([[49]]));
        assert_eq!(status["value"]["state"]["last_status"], json!(1));
    }

    #[test]
    fn arithmetic_error_retains_prior_assignment() {
        let outcome = actual(&generate(43, 0).unwrap()).unwrap();

        assert_eq!(outcome["tag"], "semantic_error");
        assert_eq!(outcome["error"]["kind"], "arithmetic");
        assert_eq!(outcome["state"]["variables"], json!([
            ["EMPTY", []],
            ["HOME", [47, 98, 114, 105, 100, 103, 101, 45, 104, 111, 109, 101]],
            ["IFS", [32, 9, 10]],
            ["VALUE", [97, 108, 112, 104, 97, 32, 98, 101, 116, 97]],
            ["x", [52]],
        ]));
    }

    #[test]
    fn arithmetic_variable_expansion_retains_dynamic_assignment() {
        let simple = actual(&generate(47, 0).unwrap()).unwrap();
        let nested = actual(&generate(48, 0).unwrap()).unwrap();
        let unreachable = generate(49, 0).unwrap();
        assert_eq!(
            unreachable.input["observations"],
            json!([{"id": "arith0", "bytes": [], "bindings": [], "error": {"kind": "arithmetic", "message": "arithmetic error: division by zero"}}])
        );
        let failed = actual(&unreachable).unwrap();

        assert_eq!(simple["value"]["fields"], json!([[52]]));
        assert_eq!(simple["value"]["state"]["variables"].as_array().unwrap().iter().find(|binding| binding[0] == "fresh"), Some(&json!(["fresh", [52]])));
        assert_eq!(nested["value"]["fields"], json!([[53]]));
        assert_eq!(nested["value"]["state"]["variables"].as_array().unwrap().iter().find(|binding| binding[0] == "nested"), Some(&json!(["nested", [52]])));
        assert_eq!(failed["tag"], "semantic_error");
        assert_eq!(failed["error"]["kind"], "arithmetic");
    }
}
