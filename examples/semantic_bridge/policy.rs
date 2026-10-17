use crate::semantic_bridge::protocol::{Family, Request, empty_state, exact_object};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::ast::{
    Assignment, CaseArm, Command, HereDocBody, ParamOp, Program, Redir, RedirKind, Word, WordPart,
};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    if index % 4 == 1 {
        return dynamic_case(index, dynamic_target((index / 4) % 10));
    }
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0xa0761d6478bd642f));
    let word = format!("item{}", random.below(100_000));
    let source = match (index / 4) % 16 {
        0 => format!("echo {word}"),
        1 => "l -l".into(),
        2 => "cd /tmp".into(),
        3 => format!("z {word}"),
        4 => format!("z {word}; echo after"),
        5 => format!("echo $(z {word})"),
        6 => "echo ${value:-fallback}".into(),
        7 => "source file".into(),
        8 => format!("printf {word} &"),
        9 => format!("f() {{ echo {word}; }}"),
        10 => format!("if true; then echo {word}; fi"),
        11 => format!("(echo {word})"),
        12 => format!("echo $value && echo {word}"),
        13 => format!("echo $(echo {word})"),
        14 => format!("alias {word}"),
        _ => format!("! echo {word}"),
    };
    let bytes = source.as_bytes().to_vec();
    let source_text = std::str::from_utf8(&bytes)
        .map_err(|_| BridgeError::new("generated policy source is not UTF-8"))?;
    let program = parse_program(source_text)?;
    Ok(Request {
        id: format!("policy-{index:05}"),
        family: Family::Policy,
        source: bytes,
        state: empty_state(),
        fuel: 256,
        input: json!({"mode": "static", "ast": program_json(&program)}),
    })
}

fn dynamic_target(index: usize) -> Vec<u8> {
    match index {
        0 => b"eval".to_vec(),
        1 => b"source".to_vec(),
        2 => b".".to_vec(),
        3 => b"z".to_vec(),
        4 => b"exit".to_vec(),
        5 => b"cd".to_vec(),
        6 => b"history".to_vec(),
        7 => b"l".to_vec(),
        8 => vec![0xff],
        _ => "\u{e080}".as_bytes().to_vec(),
    }
}

fn dynamic_case(index: usize, target: Vec<u8>) -> BridgeResult<Request> {
    let mut state = empty_state();
    state["variables"] = json!([["TARGET", target]]);
    Ok(Request {
        id: format!("policy-{index:05}"),
        family: Family::Policy,
        source: b"\"$TARGET\"".to_vec(),
        state,
        fuel: 256,
        input: json!({"mode": "dynamic", "target": target}),
    })
}

fn parse_program(source: &str) -> BridgeResult<Program> {
    Parser::new(source)
        .parse()
        .map_err(|error| BridgeError::new(format!("generated policy source did not parse: {error}")))
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    if request.input["mode"] == "dynamic" {
        let target = require_policy_bytes(&request.input["target"])?;
        let allowed = ish::dispatch::check_expanded_command(&ShellBytes::from_vec(target)).is_ok();
        return Ok(json!({
            "tag": "success",
            "value": {"allowed": allowed, "status": if allowed { 0 } else { 2 }},
        }));
    }
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("policy source is not UTF-8"))?;
    let result = ish::dispatch::plan(source);
    let value = match result {
        Ok(ish::dispatch::Plan::Epsh(_)) => json!({
            "tag": "success",
            "value": {"route": "epsh", "builtin": null, "rejection": null},
        }),
        Ok(ish::dispatch::Plan::Builtin { name, .. }) => json!({
            "tag": "success",
            "value": {"route": "builtin", "builtin": name, "rejection": null},
        }),
        Err(ish::dispatch::Rejection::Syntax(message)) => json!({
            "tag": "semantic_error",
            "error": {"kind": "syntax", "message": message},
            "state": request.state,
            "trace": [],
        }),
        Err(rejection) => {
            let reason = rejection_name(&rejection).ok_or_else(|| {
                BridgeError::new("expanded builtin rejection cannot come from static planning")
            })?;
            json!({
                "tag": "success",
                "value": {"route": "reject", "builtin": null, "rejection": reason},
            })
        }
    };
    Ok(value)
}

fn rejection_name(rejection: &ish::dispatch::Rejection) -> Option<&'static str> {
    Some(match rejection {
        ish::dispatch::Rejection::Syntax(_) => "syntax",
        ish::dispatch::Rejection::Background => "background",
        ish::dispatch::Rejection::Compound(_) => "compound",
        ish::dispatch::Rejection::FunctionDefinition => "function_definition",
        ish::dispatch::Rejection::Source(_) => "source",
        ish::dispatch::Rejection::ParameterOperator => "parameter_operator",
        ish::dispatch::Rejection::ShellBuiltinOutsideLine(_) => "shell_builtin_outside_line",
        ish::dispatch::Rejection::ExpandedShellBuiltin(_) => return None,
    })
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    let mut altered = outcome.clone();
    if altered["tag"] != "success" {
        return Err(BridgeError::new("policy negative control needs success"));
    }
    if let Some(allowed) = altered["value"]["allowed"].as_bool() {
        altered["value"]["allowed"] = json!(!allowed);
        altered["value"]["status"] = json!(if allowed { 2 } else { 0 });
        return Ok(altered);
    }
    altered["value"]["route"] = match altered["value"]["route"].as_str() {
        Some("epsh") => json!("builtin"),
        Some("builtin") => json!("epsh"),
        Some("reject") => json!("epsh"),
        _ => return Err(BridgeError::new("policy route is unknown")),
    };
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    if request.input["mode"] == "dynamic" {
        return [b"eval".to_vec(), b"cd".to_vec(), vec![0xff]]
            .into_iter()
            .enumerate()
            .map(|(index, target)| {
                let mut candidate = dynamic_case(index, target)?;
                candidate.id = request.id.clone();
                Ok(candidate)
            })
            .collect();
    }
    ["echo x", "z x", "cd /tmp", "echo $(z x)"]
        .into_iter()
        .map(|source| {
            let program = parse_program(source)?;
            let mut candidate = request.clone();
            candidate.source = source.as_bytes().to_vec();
            candidate.input["ast"] = program_json(&program);
            Ok(candidate)
        })
        .collect()
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    if request.input["mode"] == "dynamic" {
        exact_object(&request.input, &["mode", "target"], "dynamic policy input")?;
        let target = require_policy_bytes(&request.input["target"])?;
        if request.state["variables"] != json!([["TARGET", target]]) {
            return Err(BridgeError::new("dynamic policy target differs from its source variable"));
        }
        if request.source != b"\"$TARGET\"" {
            return Err(BridgeError::new("dynamic policy source is not the generated target word"));
        }
        let source = std::str::from_utf8(&request.source)
            .map_err(|_| BridgeError::new("dynamic policy source is not UTF-8"))?;
        let program = parse_program(source)?;
        let [Command::Simple { assigns, args, redirs, .. }] = program.commands.as_slice() else {
            return Err(BridgeError::new("dynamic policy source is not a simple command"));
        };
        if !assigns.is_empty() || !redirs.is_empty() || args.len() != 1
            || !matches!(args[0].parts.as_slice(), [epsh::ast::WordPart::DoubleQuoted(parts)]
                if matches!(parts.as_slice(), [epsh::ast::WordPart::Param(parameter)]
                    if parameter.name == "TARGET" && matches!(parameter.op, ParamOp::Normal)))
        {
            return Err(BridgeError::new("dynamic policy source does not expand TARGET as the command"));
        }
        return Ok(());
    }
    exact_object(&request.input, &["mode", "ast"], "static policy input")?;
    if request.source.len() > 256 {
        return Err(BridgeError::new("generated policy source exceeds 256 bytes"));
    }
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("policy source is not UTF-8"))?;
    let _ = parse_program(source)?;
    Ok(())
}

fn require_policy_bytes(value: &Value) -> BridgeResult<Vec<u8>> {
    crate::semantic_bridge::protocol::require_bytes(value, "dynamic policy target")
}

pub(crate) fn verify_runtime_hook() -> BridgeResult<()> {
    let redirection_attempts = Arc::new(AtomicUsize::new(0));
    let observed_redirections = Arc::clone(&redirection_attempts);
    let mut shell = epsh::eval::Shell::builder()
        .env_clear()
        .stdout_sink(Arc::new(Mutex::new(std::io::sink())))
        .stderr_sink(Arc::new(Mutex::new(std::io::sink())))
        .redirect_open_handler(Box::new(move |_| {
            observed_redirections.fetch_add(1, Ordering::Relaxed);
            Some(Err(std::io::Error::other("policy probe reached redirection")))
        }))
        .build();
    shell.set_var("TARGET", "eval").map_err(BridgeError::new)?;
    shell
        .set_var("POLICY_BRIDGE_MARKER", "before")
        .map_err(BridgeError::new)?;
    shell.set_command_policy(Box::new(|target| {
        ish::dispatch::check_expanded_command(target)
            .err()
            .map(|rejection| rejection.to_string())
    }));
    for source in [
        "POLICY_PREFIX=$((POLICY_BRIDGE_MARKER=9)) \"$TARGET\" POLICY_BRIDGE_MARKER=after 8>policy-bridge-redirection-probe",
        "POLICY_PREFIX=$((POLICY_BRIDGE_MARKER=9)) command \"$TARGET\" POLICY_BRIDGE_MARKER=after 8>policy-bridge-redirection-probe",
    ] {
        let program = Parser::new(source)
            .parse()
            .map_err(|error| BridgeError::new(format!("runtime policy probe did not parse: {error}")))?;
        if program.commands.len() != 1 {
            return Err(BridgeError::new("runtime policy probe is not one command"));
        }
        let status = shell.run_program(&program);
        if status.code() != 2
            || shell.get_var("POLICY_BRIDGE_MARKER") != Some("before")
            || shell.get_var("POLICY_PREFIX").is_some()
            || redirection_attempts.load(Ordering::Relaxed) != 0
        {
            return Err(BridgeError::new(
                "runtime policy hook did not reject eval before assignments and redirections",
            ));
        }
    }
    Ok(())
}

fn program_json(program: &Program) -> Value {
    json!(program.commands.iter().map(command_json).collect::<Vec<_>>())
}

fn command_json(command: &Command) -> Value {
    match command {
        Command::Simple { assigns, args, redirs, .. } => json!([
            "simple",
            assigns.iter().map(assignment_json).collect::<Vec<_>>(),
            args.iter().map(word_json).collect::<Vec<_>>(),
            redirs.iter().map(redirection_json).collect::<Vec<_>>(),
        ]),
        Command::Pipeline { commands, bang, .. } => json!([
            "pipeline",
            commands.iter().map(command_json).collect::<Vec<_>>(),
            bang,
        ]),
        Command::And(left, right) => json!(["and", command_json(left), command_json(right)]),
        Command::Or(left, right) => json!(["or", command_json(left), command_json(right)]),
        Command::Sequence(left, right) => {
            json!(["sequence", command_json(left), command_json(right)])
        }
        Command::Subshell { body, redirs, .. } => json!([
            "subshell",
            command_json(body),
            redirs.iter().map(redirection_json).collect::<Vec<_>>(),
        ]),
        Command::BraceGroup { body, redirs, .. } => json!([
            "brace_group",
            command_json(body),
            redirs.iter().map(redirection_json).collect::<Vec<_>>(),
        ]),
        Command::If { cond, then_part, else_part, .. } => json!([
            "if",
            command_json(cond),
            command_json(then_part),
            else_part.as_ref().map(|body| command_json(body)).unwrap_or(Value::Null),
        ]),
        Command::While { cond, body, .. } => json!(["while", command_json(cond), command_json(body)]),
        Command::Until { cond, body, .. } => json!(["until", command_json(cond), command_json(body)]),
        Command::For { var, words, body, .. } => json!([
            "for",
            var,
            words
                .as_ref()
                .map(|items| Value::Array(items.iter().map(word_json).collect()))
                .unwrap_or(Value::Null),
            command_json(body),
        ]),
        Command::Case { word, arms, .. } => json!([
            "case",
            word_json(word),
            arms.iter().map(case_arm_json).collect::<Vec<_>>(),
        ]),
        Command::FuncDef { name, body, .. } => json!(["func_def", name, command_json(body)]),
        Command::Not(body) => json!(["not", command_json(body)]),
        Command::Background { cmd, redirs } => json!([
            "background",
            command_json(cmd),
            redirs.iter().map(redirection_json).collect::<Vec<_>>(),
        ]),
    }
}

fn word_json(word: &Word) -> Value {
    json!(word.parts.iter().map(part_json).collect::<Vec<_>>())
}

fn part_json(part: &WordPart) -> Value {
    match part {
        WordPart::Literal(bytes) => json!(["literal", bytes.as_bytes()]),
        WordPart::SingleQuoted(bytes) => json!(["single_quoted", bytes.as_bytes()]),
        WordPart::DoubleQuoted(parts) => {
            json!(["double_quoted", parts.iter().map(part_json).collect::<Vec<_>>()])
        }
        WordPart::Param(parameter) => json!(["param", parameter.name, param_op_json(&parameter.op)]),
        WordPart::CmdSubst(command) => json!(["cmd_subst", command_json(command)]),
        WordPart::Backtick(command) => json!(["backtick", command_json(command)]),
        WordPart::Arith(parts) => json!(["arith", parts.iter().map(part_json).collect::<Vec<_>>()]),
        WordPart::Tilde(user) => json!(["tilde", user.as_bytes()]),
    }
}

fn param_op_json(operator: &ParamOp) -> Value {
    match operator {
        ParamOp::Normal => json!(["normal"]),
        ParamOp::Length => json!(["length"]),
        ParamOp::Default { colon, word } => json!(["default", colon, word.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::Assign { colon, word } => json!(["assign", colon, word.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::Error { colon, word } => json!(["error", colon, word.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::Alternative { colon, word } => json!(["alternative", colon, word.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::TrimSuffixSmall(parts) => json!(["trim_suffix_small", parts.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::TrimSuffixLarge(parts) => json!(["trim_suffix_large", parts.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::TrimPrefixSmall(parts) => json!(["trim_prefix_small", parts.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::TrimPrefixLarge(parts) => json!(["trim_prefix_large", parts.iter().map(part_json).collect::<Vec<_>>()]),
        ParamOp::BadSubst => json!(["bad_subst"]),
    }
}

fn assignment_json(assignment: &Assignment) -> Value {
    json!([assignment.name, word_json(&assignment.value)])
}

fn redirection_json(redirection: &Redir) -> Value {
    json!([redirection.fd, redirection_kind_json(&redirection.kind)])
}

fn redirection_kind_json(kind: &RedirKind) -> Value {
    match kind {
        RedirKind::Input(word) => json!(["input", word_json(word)]),
        RedirKind::Output(word) => json!(["output", word_json(word)]),
        RedirKind::Clobber(word) => json!(["clobber", word_json(word)]),
        RedirKind::Append(word) => json!(["append", word_json(word)]),
        RedirKind::ReadWrite(word) => json!(["read_write", word_json(word)]),
        RedirKind::DupInput(word) => json!(["dup_input", word_json(word)]),
        RedirKind::DupOutput(word) => json!(["dup_output", word_json(word)]),
        RedirKind::HereDoc(body) => json!(["here_doc", here_doc_json(body)]),
        RedirKind::HereDocStrip(body) => json!(["here_doc_strip", here_doc_json(body)]),
    }
}

fn here_doc_json(body: &HereDocBody) -> Value {
    match body {
        HereDocBody::Literal(bytes) => json!(["literal", bytes.as_bytes()]),
        HereDocBody::Parsed(parts) => json!(["parsed", parts.iter().map(part_json).collect::<Vec<_>>()]),
    }
}

fn case_arm_json(arm: &CaseArm) -> Value {
    json!([
        arm.patterns.iter().map(word_json).collect::<Vec<_>>(),
        arm.body.as_ref().map(command_json).unwrap_or(Value::Null),
    ])
}
