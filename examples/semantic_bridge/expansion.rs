use crate::semantic_bridge::protocol::{Family, Request, empty_state, exact_object, require_bytes};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::ast::{Command, ParamOp, Program, Word, WordPart};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0xa0761d6478bd642f));
    let source = match index % 20 {
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
        _ => {
            let suffix = random.below(100_000);
            format!("echo bridge-{suffix:05}-*.txt")
        }
    };
    let program = parse_program(&source)?;
    let words = command_words(&program)?;
    let model_words = words.iter().map(word_json).collect::<BridgeResult<Vec<_>>>()?;
    let mut state = empty_state();
    state["variables"] = json!([
        ["EMPTY", []],
        ["HOME", [47, 98, 114, 105, 100, 103, 101, 45, 104, 111, 109, 101]],
        ["IFS", [32, 9, 10]],
        ["VALUE", [97, 108, 112, 104, 97, 32, 98, 101, 116, 97]],
    ]);
    state["positional"] = if index % 20 == 7 {
        json!([[111, 110, 101], [], [116, 104, 114, 101, 101]])
    } else {
        json!([])
    };
    let observations = match index % 20 {
        9 => json!([{"id": "sub0", "bytes": [102, 105, 120, 116, 117, 114, 101, 10]}]),
        10 => json!([{"id": "arith0", "bytes": [51]}]),
        _ => json!([]),
    };
    let globs = match index % 20 {
        11 => json!([glob_json(
            b"bridge-*.txt",
            vec![false; b"bridge-*.txt".len()],
            &[b"bridge-a.txt", b"bridge-b.txt"],
        )]),
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
    let model_words = words.iter().map(word_json).collect::<BridgeResult<Vec<_>>>()?;
    if request.input["words"] != json!(model_words) {
        return Err(BridgeError::new("expansion word oracle does not match parsed source"));
    }
    let observations = request.input["observations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("expansion observations are not an array"))?;
    for observation in observations {
        exact_object(observation, &["id", "bytes"], "expansion observation")?;
        if observation["id"].as_str().is_none() {
            return Err(BridgeError::new("expansion observation ID is not text"));
        }
        require_bytes(&observation["bytes"], "expansion observation")?;
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
        validate_word_effects(&word)?;
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
    let fields = with_glob_directory(globs, |cwd| {
        let mut shell = epsh::eval::Shell::builder()
            .cwd(cwd.to_path_buf())
            .env_clear()
            .build();
        let variables = request.state["variables"]
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion variables are not an array"))?;
        for binding in variables {
            set_variable(&mut shell, binding)?;
        }
        let exported = request.state["exported"]
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion exported variables are not an array"))?;
        for binding in exported {
            let name = binding[0]
                .as_str()
                .ok_or_else(|| BridgeError::new("expansion exported name is not text"))?;
            if shell.vars().get_bytes(name).is_none() {
                set_variable(&mut shell, binding)?;
            }
        }
        let positional = request.state["positional"]
            .as_array()
            .ok_or_else(|| BridgeError::new("expansion positional state is not an array"))?
            .iter()
            .map(|bytes| require_bytes(bytes, "expansion positional state").map(ShellBytes::from_vec))
            .collect::<BridgeResult<Vec<_>>>()?;
        let mut arguments = vec![ShellBytes::from("semantic-bridge")];
        arguments.extend(positional);
        shell.set_args_bytes(&arguments);
        let mut fields = Vec::new();
        for word in &words {
            let expanded = epsh::expand::expand_word_to_fields(word, &mut shell)
                .map_err(|error| BridgeError::new(format!("production expansion failed: {error}")))?;
            fields.extend(expanded.into_iter().map(|field| field.into_bytes()));
        }
        Ok(fields)
    })?;
    Ok(json!({"tag": "success", "value": {"fields": fields}}))
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

fn safe_fixture_name(bytes: &[u8]) -> BridgeResult<&str> {
    let name = std::str::from_utf8(bytes)
        .map_err(|_| BridgeError::new("expansion fixture filename is not UTF-8"))?;
    let mut components = Path::new(name).components();
    if components.next().is_none_or(|part| !matches!(part, Component::Normal(_)))
        || components.next().is_some()
        || name.is_empty()
        || name.as_bytes().contains(&0)
    {
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
    for index in [0, 1, 3, 11, 12, 13] {
        let mut candidate = generate(index, 0)?;
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
        validate_word_effects(word)?;
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

fn validate_word_effects(word: &Word) -> BridgeResult<()> {
    for part in &word.parts {
        match part {
            WordPart::DoubleQuoted(parts) => validate_parts_effects(parts)?,
            WordPart::CmdSubst(command) | WordPart::Backtick(command) => {
                let Command::Simple { assigns, args, redirs, .. } = command.as_ref() else {
                    return Err(BridgeError::new("expansion fixture substitution must be a simple echo"));
                };
                if !assigns.is_empty()
                    || !redirs.is_empty()
                    || args.is_empty()
                    || static_word(&args[0]).as_deref() != Some(b"echo")
                    || args.iter().any(|word| static_word(word).is_none())
                {
                    return Err(BridgeError::new("expansion fixture substitution is not a safe echo"));
                }
            }
            WordPart::Arith(parts) => validate_parts_effects(parts)?,
            WordPart::Param(parameter) => match &parameter.op {
                ParamOp::Normal => {}
                ParamOp::Default { word, .. } => validate_parts_effects(word)?,
                _ => return Err(BridgeError::new("expansion fixture uses an unsupported parameter operator")),
            },
            WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => {}
        }
    }
    Ok(())
}

fn validate_parts_effects(parts: &[WordPart]) -> BridgeResult<()> {
    for part in parts {
        let word = Word { parts: vec![part.clone()], span: Default::default() };
        validate_word_effects(&word)?;
    }
    Ok(())
}

fn word_json(word: &Word) -> BridgeResult<Value> {
    Ok(Value::Array(parts_json(&word.parts, false)?))
}

fn parts_json(parts: &[WordPart], quoted: bool) -> BridgeResult<Vec<Value>> {
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
            WordPart::DoubleQuoted(inner) => result.extend(parts_json(inner, true)?),
            WordPart::Param(parameter)
                if parameter.name == "@" && matches!(parameter.op, ParamOp::Normal) && quoted =>
            {
                result.push(json!({"tag": "at"}));
            }
            WordPart::Param(parameter) => {
                let default = match &parameter.op {
                    ParamOp::Normal => Value::Null,
                    ParamOp::Default { colon, word } => json!({"colon": colon, "bytes": static_parts(word)
                        .ok_or_else(|| BridgeError::new("parameter default is not literal"))?}),
                    _ => return Err(BridgeError::new("unsupported expansion parameter operator")),
                };
                result.push(json!({
                    "tag": "parameter",
                    "name": parameter.name,
                    "quoted": quoted,
                    "default": default,
                }));
            }
            WordPart::Tilde(user) => result.push(json!({"tag": "tilde", "user": user.as_bytes()})),
            WordPart::CmdSubst(_) | WordPart::Backtick(_) => result.push(json!({
                "tag": "substitution",
                "id": "sub0",
                "quoted": quoted,
            })),
            WordPart::Arith(_) => result.push(json!({
                "tag": "arithmetic",
                "id": "arith0",
                "quoted": quoted,
            })),
        }
    }
    Ok(result)
}
