use crate::semantic_bridge::protocol::{Family, Request, empty_state, exact_object};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::ast::{
    Assignment, CaseArm, Command, HereDocBody, ParamOp, Program, Redir, RedirKind, Word, WordPart,
};
use epsh::eval::{CommandPlacement, Shell};
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    if index % 16 == 5 {
        return placement_case(index, placement_target((index / 96) % 3));
    }
    if index % 4 == 1 {
        return dynamic_case(index, dynamic_target((index / 4) % 12));
    }
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0xa0761d6478bd642f));
    let word = format!("item{}", random.below(100_000));
    let source = match (index / 4) % 20 {
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
        15 => format!("! echo {word}"),
        16 => "cp source destination".into(),
        17 => "mv source destination".into(),
        18 => "printf input | cp source destination".into(),
        _ => "echo \"$(cp source destination)\"".into(),
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
        8 => b"cp".to_vec(),
        9 => b"mv".to_vec(),
        10 => vec![0xff],
        _ => "\u{e080}".as_bytes().to_vec(),
    }
}

fn placement_target(index: usize) -> Vec<u8> {
    match index {
        0 => b"cp".to_vec(),
        1 => b"mv".to_vec(),
        _ => vec![0xff],
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

fn placement_case(index: usize, target: Vec<u8>) -> BridgeResult<Request> {
    let context = match (index / 16) % 6 {
        0 => "parent",
        1 => "negated-parent",
        2 => "pipeline",
        3 => "negated-pipeline",
        4 => "substitution",
        _ => "negated-substitution",
    };
    placement_request(format!("policy-{index:05}"), target, context)
}

fn placement_request(id: String, target: Vec<u8>, context: &str) -> BridgeResult<Request> {
    let source = placement_source(context)?;
    let mut state = empty_state();
    state["variables"] = json!([["TARGET", target.clone()]]);
    Ok(Request {
        id,
        family: Family::Policy,
        source: source.into_bytes(),
        state,
        fuel: 256,
        input: json!({"mode": "placement", "target": target, "context": context}),
    })
}

fn placement_source(context: &str) -> BridgeResult<String> {
    let command = "command command \"$TARGET\" source destination > placement-redirection";
    Ok(match context {
        "parent" => command.to_owned(),
        "negated-parent" => format!("! {command}"),
        "pipeline" => format!("printf input | {command}"),
        "negated-pipeline" => format!("! printf input | {command}"),
        "substitution" => format!("value=$({command})"),
        "negated-substitution" => format!("value=$(! {command})"),
        _ => return Err(BridgeError::new("unknown placement context")),
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
    if request.input["mode"] == "placement" {
        return actual_placement(request);
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
    if request.input["mode"] == "placement" {
        return [
            (b"cp".to_vec(), "parent"),
            (b"cp".to_vec(), "pipeline"),
            (vec![0xff], "substitution"),
        ]
        .into_iter()
        .map(|(target, context)| placement_request(request.id.clone(), target, context))
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
    if request.input["mode"] == "placement" {
        exact_object(&request.input, &["mode", "target", "context"], "placement policy input")?;
        let target = require_policy_bytes(&request.input["target"])?;
        let context = request.input["context"]
            .as_str()
            .ok_or_else(|| BridgeError::new("placement context is not a string"))?;
        let expected_source = placement_source(context)?;
        if request.source != expected_source.as_bytes() {
            return Err(BridgeError::new("placement source does not match its context"));
        }
        if request.state["variables"] != json!([["TARGET", target]]) {
            return Err(BridgeError::new("placement target differs from TARGET bytes"));
        }
        let _ = parse_program(&expected_source)?;
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

struct PolicyProbeDirectory(Option<PathBuf>);

impl PolicyProbeDirectory {
    fn create() -> BridgeResult<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work/tmp");
        fs::create_dir_all(&root)
            .map_err(|error| BridgeError::new(format!("cannot create {}: {error}", root.display())))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| BridgeError::new(format!("system clock is before the Unix epoch: {error}")))?
            .as_nanos();
        let path = root.join(format!("semantic-policy-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)
            .map_err(|error| BridgeError::new(format!("cannot create {}: {error}", path.display())))?;
        Ok(Self(Some(path)))
    }

    fn path(&self) -> &PathBuf {
        self.0.as_ref().expect("policy probe directory is active")
    }

    fn remove(mut self) -> BridgeResult<()> {
        let path = self.0.take().expect("policy probe directory is active");
        fs::remove_dir_all(&path)
            .map_err(|error| BridgeError::new(format!("cannot remove {}: {error}", path.display())))
    }
}

impl Drop for PolicyProbeDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.0.take()
            && let Err(error) = fs::remove_dir_all(&path)
        {
            eprintln!("cannot remove {}: {error}", path.display());
        }
    }
}

fn actual_placement(request: &Request) -> BridgeResult<Value> {
    exact_object(&request.input, &["mode", "target", "context"], "placement policy input")?;
    let target = require_policy_bytes(&request.input["target"])?;
    let context = request.input["context"]
        .as_str()
        .ok_or_else(|| BridgeError::new("placement context is not a string"))?;
    let (parent_context, negated) = placement_properties(context)?;
    let dynamic_allowed =
        ish::dispatch::check_expanded_command(&ShellBytes::from_vec(target.clone())).is_ok();
    let target_allowed = parent_context && dynamic_allowed;
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("placement source is not UTF-8"))?;
    let program = Parser::new(source)
        .parse()
        .map_err(|error| BridgeError::new(format!("placement source did not parse: {error}")))?;
    let directory = PolicyProbeDirectory::create()?;
    let policy_marker = directory.path().join("policy-called");
    let handler_marker = directory.path().join("handler-called");
    let redirection_path = directory.path().join("placement-redirection");
    let observed_target = target.clone();
    let mut shell = Shell::builder()
        .cwd(directory.path().clone())
        .env_clear()
        .stdout_sink(Arc::new(Mutex::new(std::io::sink())))
        .stderr_sink(Arc::new(Mutex::new(std::io::sink())))
        .build();
    shell
        .set_var_bytes("TARGET", ShellBytes::from_vec(target.clone()))
        .map_err(BridgeError::new)?;
    shell.set_command_placement(
        ShellBytes::from_vec(target.clone()),
        CommandPlacement::ParentProcess,
    );
    let policy_marker_copy = policy_marker.clone();
    let policy_target = target.clone();
    shell.set_command_policy(Box::new(move |name| {
        if name.as_bytes() == policy_target {
            fs::write(&policy_marker_copy, b"called").expect("policy marker write succeeds");
        }
        ish::dispatch::check_expanded_command(name)
            .err()
            .map(|rejection| rejection.to_string())
    }));
    let handler_marker_copy = handler_marker.clone();
    shell.set_external_handler(Box::new(move |args, _environment| {
        if args
            .first()
            .is_some_and(|name| name.as_bytes() == observed_target)
        {
            fs::write(&handler_marker_copy, b"called").expect("handler marker write succeeds");
        }
        Ok(epsh::error::ExitStatus::SUCCESS)
    }));
    let status = shell.run_program(&program).code();
    let target_status = if target_allowed { 0 } else { 2 };
    let expected_status = enclosing_placement_status(negated, target_status);
    let policy_called = policy_marker.exists();
    let handler_called = handler_marker.exists();
    let redirected = redirection_path.exists();
    if status != expected_status
        || policy_called != target_allowed
        || handler_called != target_allowed
        || redirected != target_allowed
    {
        return Err(BridgeError::new(format!(
            "placement runtime mismatch for {context} target {target:?}: status={status}, policy={policy_called}, handler={handler_called}, redirection={redirected}"
        )));
    }
    let class = std::str::from_utf8(&target)
        .ok()
        .and_then(ish::builtin::lookup)
        .map(|builtin| match builtin.exec {
            ish::builtin::Exec::InShell => "in_shell",
            ish::builtin::Exec::Hybrid => "hybrid",
            ish::builtin::Exec::Foreground => "foreground",
            ish::builtin::Exec::Native => "native",
        });
    directory.remove()?;
    Ok(json!({
        "tag": "success",
        "value": {
            "allowed": target_allowed,
            "status": status,
            "class": class,
        },
    }))
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
    verify_runtime_placement()
}

fn verify_runtime_placement() -> BridgeResult<()> {
    let targets = [b"cp".to_vec(), b"mv".to_vec(), vec![0xff]];
    let contexts = [
        "parent",
        "negated-parent",
        "pipeline",
        "negated-pipeline",
        "substitution",
        "negated-substitution",
    ];
    let mut index = 0;
    for target in targets {
        for context in contexts {
            let request = placement_request(
                format!("policy-placement-probe-{index}"),
                target.clone(),
                context,
            )?;
            validate_case(&request)?;
            let outcome = actual_placement(&request)?;
            if outcome["tag"] != "success"
                || outcome["value"]["allowed"] != json!(placement_properties(context)?.0)
            {
                return Err(BridgeError::new("runtime placement probe returned the wrong context result"));
            }
            index += 1;
        }
    }
    for source in [
        "cp source destination",
        "mv source destination",
        "! cp source destination",
        "! mv source destination",
        "printf input | cp source destination",
        "value=$(mv source destination)",
    ] {
        if !matches!(ish::dispatch::plan(source), Ok(ish::dispatch::Plan::Epsh(_))) {
            return Err(BridgeError::new(format!(
                "static foreground target did not remain in epsh: {source}"
            )));
        }
    }
    Ok(())
}

fn placement_properties(context: &str) -> BridgeResult<(bool, bool)> {
    Ok(match context {
        "parent" => (true, false),
        "negated-parent" => (true, true),
        "pipeline" => (false, false),
        "negated-pipeline" => (false, true),
        "substitution" => (false, false),
        "negated-substitution" => (false, true),
        _ => return Err(BridgeError::new("unknown placement context")),
    })
}

fn enclosing_placement_status(negated: bool, target_status: i32) -> i32 {
    if negated && target_status != 130 {
        if target_status == 0 { 1 } else { 0 }
    } else {
        target_status
    }
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
