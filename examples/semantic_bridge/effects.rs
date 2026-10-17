use crate::semantic_bridge::protocol::{
    Family, Request, empty_state, exact_object, require_bytes,
};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::eval::Shell;
use epsh::parser::Parser;
use epsh::shell_bytes::ShellBytes;
use epsh::var::Variables;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WORKER_ARGUMENT: &str = "--effects-worker";
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_WORKER_OUTPUT_BYTES: usize = 65_536;
const WORKER_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_DESCRIPTOR: u64 = 3;
const MAX_DESCRIPTOR: u64 = 9;
const MIN_SAVED_COPY: u64 = 32;
const MAX_SAVED_COPY: u64 = 63;
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generate(index: usize, seed: u64) -> BridgeResult<Request> {
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0x8ebc_6af0_9c88_c6e3));
    let (state, input) = match index {
        0 => (empty_state(), json!({"mode": "bytes", "bytes": []})),
        1 => (
            empty_state(),
            json!({"mode": "bytes", "bytes": (0u8..=u8::MAX).collect::<Vec<_>>() }),
        ),
        2 => {
            let mut bytes = vec![b'a', 0, 0xee, 0x82, 0x80, 0xef, 0x80, 0x80, 0xff, 0x80];
            bytes.extend((0..24).map(|_| random.next() as u8));
            (empty_state(), json!({"mode": "bytes", "bytes": bytes}))
        }
        3 => environment_case(
            vec![("A", b"2".to_vec()), ("EXPORTED", vec![0xef, 0x80, 0x80])],
            vec![("A", b"3".to_vec()), ("PREFIX", b"assigned".to_vec())],
            vec![(b"A".to_vec(), b"1".to_vec()), (b"INHERITED".to_vec(), b"kept".to_vec())],
        ),
        4 => environment_case(
            vec![("A", b"exported".to_vec())],
            Vec::new(),
            vec![
                (b"A".to_vec(), b"inherited".to_vec()),
                (b"0RAW".to_vec(), b"non-shell".to_vec()),
                (vec![0xff, b'R', b'A', b'W'], vec![0x80, 0xff]),
                ("PUA-\u{e080}".as_bytes().to_vec(), "\u{e080}".as_bytes().to_vec()),
            ],
        ),
        5 => environment_case(
            vec![("BASE", vec![0xef, 0x80, 0x80])],
            vec![("BASE", b"assigned".to_vec()), ("NEW", b"value".to_vec())],
            vec![(b"BASE".to_vec(), b"inherited".to_vec()), (b"RAW".to_vec(), vec![0x80, 0xff])],
        ),
        6 => environment_case(
            vec![("FIRST", b"exported".to_vec()), ("SECOND", b"two".to_vec())],
            vec![("FIRST", b"prefix".to_vec())],
            vec![(b"FIRST".to_vec(), b"inherited".to_vec()), (b"UNTOUCHED".to_vec(), b"kept".to_vec())],
        ),
        7 => environment_case(
            vec![("RAW_VALUE", vec![0x80, 0xff])],
            Vec::new(),
            vec![(b"RAW_VALUE".to_vec(), b"inherited".to_vec())],
        ),
        8 => cwd_case(true, (index % 3) as u64),
        9 => cwd_case(false, (index % 3) as u64),
        10 => redirection_case(false, 0),
        11 => redirection_case(false, 1),
        12 => redirection_case(true, 0),
        13 => redirection_case(true, 1),
        _ => match random.below(4) {
            0 => {
                let length = random.below(64) as usize;
                let bytes: Vec<u8> = (0..length).map(|_| random.next() as u8).collect();
                (empty_state(), json!({"mode": "bytes", "bytes": bytes}))
            }
            1 => {
                let raw_name = vec![0xff, b'A' + random.below(26) as u8];
                let raw_value = vec![random.next() as u8 | 1];
                let raw = vec![
                    (b"BASE".to_vec(), b"inherited".to_vec()),
                    (raw_name, raw_value),
                ];
                let assignments = if random.below(2) == 0 {
                    vec![("BASE", b"assigned".to_vec())]
                } else {
                    Vec::new()
                };
                environment_case(
                    vec![("BASE", b"exported".to_vec())],
                    assignments,
                    raw,
                )
            }
            2 => cwd_case(random.below(2) == 0, random.below(3)),
            _ => redirection_case(random.below(2) == 0, random.below(2) as usize),
        },
    };
    Ok(Request {
        id: format!("effects-{index:05}"),
        family: Family::Effects,
        source: Vec::new(),
        state,
        fuel: 128,
        input,
    })
}

pub(crate) fn boundary_cases() -> Vec<Request> {
    let invalid_inherited_name = environment_case(
        Vec::new(),
        Vec::new(),
        vec![(b"BAD=NAME".to_vec(), b"value".to_vec())],
    );
    let invalid_inherited_value = environment_case(
        Vec::new(),
        Vec::new(),
        vec![(b"BAD_VALUE".to_vec(), vec![b'a', 0, b'b'])],
    );
    let invalid_exported_name = environment_case(
        vec![("BAD-NAME", b"value".to_vec())],
        Vec::new(),
        Vec::new(),
    );
    let (fuel_state, fuel_input) = redirection_case(false, 0);
    [
        ("effects-domain-inherited-name", invalid_inherited_name, 128),
        ("effects-domain-inherited-value", invalid_inherited_value, 128),
        ("effects-domain-exported-name", invalid_exported_name, 128),
        ("effects-fuel-boundary", (fuel_state, fuel_input), 0),
    ]
    .into_iter()
    .map(|(id, (state, input), fuel)| Request {
        id: id.to_string(),
        family: Family::Effects,
        source: Vec::new(),
        state,
        fuel,
        input,
    })
    .collect()
}

fn environment_case(
    exported: Vec<(&str, Vec<u8>)>,
    assignments: Vec<(&str, Vec<u8>)>,
    inherited: Vec<(Vec<u8>, Vec<u8>)>,
) -> (Value, Value) {
    let mut state = empty_state();
    state["variables"] = json!([["HIDDEN", [112, 114, 105, 118, 97, 116, 101]]]);
    state["exported"] = json!(exported
        .into_iter()
        .map(|(name, value)| json!([name, value]))
        .collect::<Vec<_>>());
    state["inherited"] = json!(inherited
        .into_iter()
        .map(|(name, value)| json!([name, value]))
        .collect::<Vec<_>>());
    let input = json!({
        "mode": "environment",
        "assignments": assignments
            .into_iter()
            .map(|(name, value)| json!([name, value]))
            .collect::<Vec<_>>(),
    });
    (state, input)
}

fn cwd_case(success: bool, shell: u64) -> (Value, Value) {
    let mut state = empty_state();
    state["cwd"] = json!(b"start".to_vec());
    (
        state,
        json!({
            "mode": "cwd",
            "shell": shell,
            "path": b"selected".to_vec(),
            "success": success,
            "other_cwds": [b"other-a".to_vec(), b"other-b".to_vec()],
        }),
    )
}

fn redirection_case(permanent: bool, variant: usize) -> (Value, Value) {
    let descriptors = json!([[3, 7], [4, 8]]);
    let operations = match (permanent, variant) {
        (false, 0) => vec![
            redirection_op(5, 32, "open", 9),
            redirection_op(6, 33, "duplicate", 3),
            redirection_op(4, 34, "close", 0),
        ],
        (false, _) => vec![
            redirection_op(5, 32, "open", 9),
            redirection_op(6, 33, "duplicate", 4),
            redirection_op(7, 34, "fail", 63),
        ],
        (true, 0) => vec![
            redirection_op(5, 32, "open", 9),
            redirection_op(6, 33, "duplicate", 5),
            redirection_op(3, 34, "close", 0),
        ],
        (true, _) => vec![
            redirection_op(5, 32, "open", 9),
            redirection_op(6, 33, "duplicate", 3),
            redirection_op(7, 34, "fail", 63),
        ],
    };
    let mut state = empty_state();
    state["cwd"] = json!(b"/".to_vec());
    (
        state,
        json!({
            "mode": "redirection",
            "descriptors": descriptors,
            "operations": operations,
            "permanent": permanent,
        }),
    )
}

fn redirection_op(target: u64, copy: u64, kind: &str, resource: u64) -> Value {
    json!({
        "target": target,
        "copy": copy,
        "kind": kind,
        "resource": resource,
        "restore": true,
    })
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    if request.family != Family::Effects {
        return Err(BridgeError::new("effects adapter received another family"));
    }
    if request.id.is_empty() {
        return Err(BridgeError::new("effects request ID is empty"));
    }
    validate_state(&request.state)?;
    let mode = request.input["mode"]
        .as_str()
        .ok_or_else(|| BridgeError::new("effects input has no mode"))?;
    match mode {
        "bytes" => {
            exact_object(&request.input, &["mode", "bytes"], "bytes input")?;
            require_bytes(&request.input["bytes"], "bytes input")?;
        }
        "environment" => validate_environment_input(request)?,
        "cwd" => validate_cwd_input(request)?,
        "redirection" => validate_redirection_input(request)?,
        _ => return Err(BridgeError::new(format!("unknown effects mode {mode:?}"))),
    }
    Ok(())
}

fn validate_state(state: &Value) -> BridgeResult<()> {
    exact_object(
        state,
        &[
            "variables",
            "exported",
            "inherited",
            "cwd",
            "positional",
            "last_status",
        ],
        "effects state",
    )?;
    decode_bindings(&state["variables"], "variables")?;
    decode_bindings(&state["exported"], "exported")?;
    decode_environment(&state["inherited"], "inherited")?;
    require_bytes(&state["cwd"], "cwd")?;
    let positional = state["positional"]
        .as_array()
        .ok_or_else(|| BridgeError::new("positional state is not an array"))?;
    for item in positional {
        require_bytes(item, "positional argument")?;
    }
    state["last_status"]
        .as_u64()
        .filter(|status| *status < 256)
        .ok_or_else(|| BridgeError::new("last status is outside 0..255"))?;
    Ok(())
}

fn validate_environment_input(request: &Request) -> BridgeResult<()> {
    exact_object(
        &request.input,
        &["mode", "assignments"],
        "environment input",
    )?;
    let _ = decode_bindings(&request.input["assignments"], "assignments")?;
    let _ = decode_bindings(&request.state["exported"], "exported")?;
    let _ = decode_environment(&request.state["inherited"], "inherited")?;
    Ok(())
}

fn validate_cwd_input(request: &Request) -> BridgeResult<()> {
    exact_object(
        &request.input,
        &["mode", "shell", "path", "success", "other_cwds"],
        "cwd input",
    )?;
    request.input["shell"]
        .as_u64()
        .filter(|shell| *shell <= 2)
        .ok_or_else(|| BridgeError::new("cwd shell is outside the generated range"))?;
    let path = require_bytes(&request.input["path"], "cwd path")?;
    let cwd = require_bytes(&request.state["cwd"], "cwd state")?;
    let other_cwds = request.input["other_cwds"]
        .as_array()
        .ok_or_else(|| BridgeError::new("other_cwds is not an array"))?;
    let others = other_cwds
        .iter()
        .map(|path| require_bytes(path, "other cwd"))
        .collect::<BridgeResult<Vec<_>>>()?;
    if others.len() != 2 {
        return Err(BridgeError::new("cwd mode supports two other shells"));
    }
    if path.contains(&0) || cwd.contains(&0) || others.iter().any(|path| path.contains(&0)) {
        return Err(BridgeError::new("cwd paths contain NUL"));
    }
    if path.len() > 120 || cwd.len() > 120 || others.iter().any(|path| path.len() > 120) {
        return Err(BridgeError::new("cwd paths exceed the supported scratch-path length"));
    }
    request.input["success"]
        .as_bool()
        .ok_or_else(|| BridgeError::new("cwd success is not boolean"))?;
    Ok(())
}

fn validate_redirection_input(request: &Request) -> BridgeResult<()> {
    exact_object(
        &request.input,
        &["mode", "descriptors", "operations", "permanent"],
        "redirection input",
    )?;
    let descriptors = decode_descriptors(&request.input["descriptors"])?;
    let operations = request.input["operations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("redirection operations are not an array"))?;
    if operations.len() > 16 {
        return Err(BridgeError::new("redirection operation count exceeds 16"));
    }
    let permanent = request.input["permanent"]
        .as_bool()
        .ok_or_else(|| BridgeError::new("redirection permanence is not boolean"))?;
    let mut open: BTreeSet<u64> = descriptors.keys().copied().collect();
    let mut copies = BTreeSet::new();
    for (index, operation) in operations.iter().enumerate() {
        exact_object(
            operation,
            &["target", "copy", "kind", "resource", "restore"],
            "redirection operation",
        )?;
        let target = require_fd(&operation["target"], "redirection target")?;
        let copy = operation["copy"]
            .as_u64()
            .filter(|copy| (MIN_SAVED_COPY..=MAX_SAVED_COPY).contains(copy))
            .ok_or_else(|| BridgeError::new("saved copy is outside the generated range"))?;
        let resource = require_fd(&operation["resource"], "redirection resource")?;
        if !(MIN_DESCRIPTOR..=MAX_DESCRIPTOR).contains(&target) || copy == target {
            return Err(BridgeError::new("redirection target is outside the supported subset"));
        }
        if operation["restore"].as_bool() != Some(true) {
            return Err(BridgeError::new("generated redirections must restore temporary descriptors"));
        }
        if !permanent {
            if open.contains(&copy) || !copies.insert(copy) {
                return Err(BridgeError::new("saved descriptor copy is not fresh"));
            }
            open.insert(copy);
        }
        match operation["kind"].as_str() {
            Some("open") => {
                if resource > 255 {
                    return Err(BridgeError::new("open resource is outside the generated range"));
                }
                open.insert(target);
            }
            Some("duplicate") => {
                if !(MIN_DESCRIPTOR..=MAX_DESCRIPTOR).contains(&resource) {
                    return Err(BridgeError::new("duplicate source is outside the supported subset"));
                }
                if !open.contains(&resource) {
                    return Err(BridgeError::new("duplicate source is outside the generated subset"));
                }
                open.insert(target);
            }
            Some("close") => {
                if !open.remove(&target) {
                    return Err(BridgeError::new("close target is outside the generated subset"));
                }
            }
            Some("fail") => {
                if open.contains(&resource) {
                    return Err(BridgeError::new("failed duplicate source is open"));
                }
                if index + 1 != operations.len() {
                    return Err(BridgeError::new("failure must end the supported redirection sequence"));
                }
            }
            Some(kind) => return Err(BridgeError::new(format!("unknown redirection kind {kind:?}"))),
            None => return Err(BridgeError::new("redirection kind is not text")),
        }
    }
    Ok(())
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    validate_case(request)?;
    let mode = request.input["mode"].as_str().unwrap_or("");
    if mode == "environment"
        && let Some(reason) = environment_domain_error(request)?
    {
        return Ok(json!({"tag": "unsupported_domain", "reason": reason}));
    }
    if request.fuel == 0
        || (mode == "redirection"
            && request.fuel <= request.input["operations"].as_array().map_or(0, Vec::len) as u64)
    {
        return Ok(json!({"tag": "fuel_exhausted"}));
    }
    if matches!(mode, "environment" | "redirection") {
        run_isolated(request)
    } else {
        actual_in_process(request)
    }
}

pub(crate) fn subprocess_worker() -> BridgeResult<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|error| BridgeError::new(format!("cannot read effects worker request: {error}")))?;
    if input.len() > MAX_REQUEST_BYTES {
        return Err(BridgeError::new("effects worker request exceeds the size limit"));
    }
    let value: Value = serde_json::from_slice(&input)
        .map_err(|error| BridgeError::new(format!("effects worker request is malformed: {error}")))?;
    let request = request_from_json(value)?;
    validate_case(&request)?;
    let outcome = if request.fuel == 0
        || (request.input["mode"] == "redirection"
            && request.fuel <= request.input["operations"].as_array().map_or(0, Vec::len) as u64)
    {
        json!({"tag": "fuel_exhausted"})
    } else if request.input["mode"] == "environment"
        && let Some(reason) = environment_domain_error(&request)?
    {
        json!({"tag": "unsupported_domain", "reason": reason})
    } else {
        actual_in_process(&request)?
    };
    std::io::stdout()
        .lock()
        .write_all(&serde_json::to_vec(&outcome).map_err(|error| {
            BridgeError::new(format!("cannot encode effects worker outcome: {error}"))
        })?)
        .map_err(|error| BridgeError::new(format!("cannot write effects worker outcome: {error}")))?;
    Ok(())
}

fn request_from_json(value: Value) -> BridgeResult<Request> {
    exact_object(
        &value,
        &["version", "id", "family", "source", "state", "fuel", "input"],
        "effects worker request",
    )?;
    if value["version"] != 1 {
        return Err(BridgeError::new("effects worker request version is unsupported"));
    }
    let id = value["id"]
        .as_str()
        .ok_or_else(|| BridgeError::new("effects worker request ID is not text"))?
        .to_string();
    let family = Family::parse(
        value["family"]
            .as_str()
            .ok_or_else(|| BridgeError::new("effects worker family is not text"))?,
    )?;
    let source = require_bytes(&value["source"], "effects worker source")?;
    let fuel = value["fuel"]
        .as_u64()
        .ok_or_else(|| BridgeError::new("effects worker fuel is not a natural number"))?;
    if !value["input"].is_object() {
        return Err(BridgeError::new("effects worker input is not an object"));
    }
    Ok(Request {
        id,
        family,
        source,
        state: value["state"].clone(),
        fuel,
        input: value["input"].clone(),
    })
}

fn run_isolated(request: &Request) -> BridgeResult<Value> {
    let executable = std::env::current_exe()
        .map_err(|error| BridgeError::new(format!("cannot locate semantic bridge executable: {error}")))?;
    let inherited = decode_environment(&request.state["inherited"], "inherited")?;
    let mut command = Command::new(executable);
    command
        .arg(WORKER_ARGUMENT)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in inherited {
        if !valid_os_environment_pair(&name, &value) {
            return Err(BridgeError::new("cannot initialize an invalid inherited environment entry"));
        }
        command.env(OsString::from_vec(name), OsString::from_vec(value));
    }
    let mut child = command
        .spawn()
        .map_err(|error| BridgeError::new(format!("cannot start effects worker: {error}")))?;
    let encoded = serde_json::to_vec(&request.to_json())
        .map_err(|error| BridgeError::new(format!("cannot encode effects worker request: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| BridgeError::new("effects worker stdin is unavailable"))?;
    if let Err(error) = stdin.write_all(&encoded) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(BridgeError::new(format!("cannot write effects worker request: {error}")));
    }
    drop(stdin);
    let status = wait_bounded(&mut child)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BridgeError::new("effects worker stdout is unavailable"))?;
    let mut output = Vec::new();
    stdout
        .take((MAX_WORKER_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|error| BridgeError::new(format!("cannot read effects worker output: {error}")))?;
    if output.len() > MAX_WORKER_OUTPUT_BYTES {
        return Err(BridgeError::new("effects worker output exceeds the size limit"));
    }
    if !status.success() {
        let mut stderr = Vec::new();
        if let Some(stream) = child.stderr.take() {
            stream
                .take((MAX_WORKER_OUTPUT_BYTES + 1) as u64)
                .read_to_end(&mut stderr)
                .map_err(|error| BridgeError::new(format!("cannot read effects worker error: {error}")))?;
        }
        return Err(BridgeError::new(format!(
            "effects worker exited with {status}: {}",
            String::from_utf8_lossy(&stderr[..stderr.len().min(MAX_WORKER_OUTPUT_BYTES)])
        )));
    }
    let outcome: Value = serde_json::from_slice(&output)
        .map_err(|error| BridgeError::new(format!("effects worker outcome is malformed: {error}")))?;
    let envelope = json!({"version": 1, "id": request.id, "outcome": outcome.clone()});
    let _ = crate::semantic_bridge::protocol::validate_response(envelope, &request.id)?;
    Ok(outcome)
}

fn wait_bounded(child: &mut Child) -> BridgeResult<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BridgeError::new(format!("cannot poll effects worker: {error}")));
            }
        }
        if start.elapsed() >= WORKER_TIMEOUT {
            child
                .kill()
                .map_err(|error| BridgeError::new(format!("cannot stop timed out effects worker: {error}")))?;
            let _ = child.wait();
            return Err(BridgeError::new("effects worker exceeded its time limit"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn actual_in_process(request: &Request) -> BridgeResult<Value> {
    match request.input["mode"].as_str().unwrap_or("") {
        "bytes" => actual_bytes(&request.input),
        "environment" => actual_environment(request),
        "cwd" => actual_cwd(request),
        "redirection" => actual_redirections(request),
        mode => Err(BridgeError::new(format!("unknown effects mode {mode:?}"))),
    }
}

fn actual_bytes(input: &Value) -> BridgeResult<Value> {
    let bytes = require_bytes(&input["bytes"], "bytes input")?;
    let shell = ShellBytes::from_vec(bytes);
    let os = shell.to_os_string();
    let raw_os = ShellBytes::from_os_str(os.as_os_str());
    let c_string = raw_os.to_cstring().is_ok();
    Ok(json!({
        "tag": "success",
        "value": {"bytes": raw_os.as_bytes(), "c_string": c_string},
    }))
}

fn actual_environment(request: &Request) -> BridgeResult<Value> {
    let variables = environment_variables(request)?;
    let assignments = decode_bindings(&request.input["assignments"], "assignments")?
        .into_iter()
        .map(|(name, value)| (name, ShellBytes::from_vec(value)))
        .collect::<Vec<_>>();
    let mut environment = variables
        .env_for_command_os(&assignments)
        .into_iter()
        .map(|(name, value)| {
            (
                ShellBytes::from_os_string(name).into_vec(),
                ShellBytes::from_os_string(value).into_vec(),
            )
        })
        .collect::<Vec<_>>();
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(json!({
        "tag": "success",
        "value": {"environment": environment},
    }))
}

fn environment_variables(request: &Request) -> BridgeResult<Variables> {
    let mut variables = Variables::new();
    for (name, value) in decode_bindings(&request.state["variables"], "variables")? {
        variables
            .set_bytes(&name, ShellBytes::from_vec(value))
            .map_err(BridgeError::new)?;
    }
    for (name, value) in decode_bindings(&request.state["exported"], "exported")? {
        variables
            .set_bytes(&name, ShellBytes::from_vec(value))
            .map_err(BridgeError::new)?;
        variables.export(&name);
    }
    Ok(variables)
}

fn environment_domain_error(request: &Request) -> BridgeResult<Option<&'static str>> {
    let assignments = decode_bindings(&request.input["assignments"], "assignments")?;
    let exported = decode_bindings(&request.state["exported"], "exported")?;
    if exported
        .iter()
        .map(|(name, _)| name)
        .chain(assignments.iter().map(|(name, _)| name))
        .any(|name| !shell_name(name))
    {
        return Ok(Some("exported and assigned names must be shell identifiers"));
    }
    let inherited = decode_environment(&request.state["inherited"], "inherited")?;
    if inherited
        .iter()
        .any(|(name, value)| !valid_os_environment_pair(name, value))
        || exported.iter().any(|(name, value)| {
            !valid_os_environment_pair(name.as_bytes(), value)
        })
        || assignments.iter().any(|(name, value)| {
            !valid_os_environment_pair(name.as_bytes(), value)
        })
    {
        return Ok(Some("environment names or values cannot cross the OS boundary"));
    }
    Ok(None)
}

fn valid_os_environment_pair(name: &[u8], value: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&0) && !name.contains(&b'=') && !value.contains(&0)
}

fn shell_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    (first.is_ascii_alphabetic() || *first == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn actual_cwd(request: &Request) -> BridgeResult<Value> {
    with_scratch(|root| {
        let cwd_bytes = require_bytes(&request.state["cwd"], "cwd state")?;
        let target_bytes = require_bytes(&request.input["path"], "cwd path")?;
        let other_bytes = request.input["other_cwds"]
            .as_array()
            .ok_or_else(|| BridgeError::new("other_cwds is not an array"))?
            .iter()
            .map(|path| require_bytes(path, "other cwd"))
            .collect::<BridgeResult<Vec<_>>>()?;
        let start = scratch_path(root, "shell", &cwd_bytes);
        let other_paths = other_bytes
            .iter()
            .map(|cwd| scratch_path(root, "shell", cwd))
            .collect::<Vec<_>>();
        let selected_path = scratch_path(root, "target", &target_bytes);
        fs::create_dir_all(&start)
            .map_err(|error| BridgeError::new(format!("cannot create cwd fixture: {error}")))?;
        for path in &other_paths {
            fs::create_dir_all(path)
                .map_err(|error| BridgeError::new(format!("cannot create other cwd fixture: {error}")))?;
        }
        let success = request.input["success"].as_bool().unwrap_or(false);
        if success {
            fs::create_dir_all(&selected_path)
                .map_err(|error| BridgeError::new(format!("cannot create selected cwd fixture: {error}")))?;
        }
        let process_cwd = std::env::current_dir()
            .map_err(|error| BridgeError::new(format!("cannot inspect process cwd: {error}")))?;
        let selected_slot = request.input["shell"].as_u64().unwrap_or(0) as usize;
        let mut shell_cwds = Vec::with_capacity(other_paths.len() + 1);
        let mut next_other = other_paths.iter();
        for slot in 0..=other_paths.len() {
            shell_cwds.push(if slot == selected_slot {
                start.clone()
            } else {
                next_other
                    .next()
                    .ok_or_else(|| BridgeError::new("cwd shell slots do not match other_cwds"))?
                    .clone()
            });
        }
        let mut shells = shell_cwds
            .iter()
            .map(|cwd| {
                Shell::builder()
                    .cwd(cwd.clone())
                    .env_clear()
                    .stderr_sink(Arc::new(Mutex::new(std::io::sink())))
                    .build()
            })
            .collect::<Vec<_>>();
        let source = format!("cd {}", shell_quote(&selected_path));
        let status = evaluate_source(&mut shells[selected_slot], &source)
            .map_err(|error| BridgeError::new(error.to_string()))?;
        if (status.code() == 0) != success {
            return Err(BridgeError::new("epsh cd outcome disagrees with the generated cwd case"));
        }
        if std::env::current_dir()
            .map_err(|error| BridgeError::new(format!("cannot inspect process cwd: {error}")))?
            != process_cwd
        {
            return Err(BridgeError::new("epsh changed the process cwd"));
        }
        let expected_selected_path = if success { &selected_path } else { &start };
        if shells[selected_slot].cwd() != expected_selected_path.as_path() {
            return Err(BridgeError::new("epsh cwd state disagrees with the real directory operation"));
        }
        for (slot, shell) in shells.iter().enumerate() {
            if slot != selected_slot && shell.cwd() != shell_cwds[slot].as_path() {
                return Err(BridgeError::new("epsh changed another shell cwd"));
            }
        }
        Ok(json!({
            "tag": "success",
            "value": {
                "cwd": if success { target_bytes } else { cwd_bytes },
                "other_cwds": other_bytes,
            },
        }))
    })
}

fn actual_redirections(request: &Request) -> BridgeResult<Value> {
    with_scratch(|root| {
    let descriptors = decode_descriptors(&request.input["descriptors"])?;
    let operations = request.input["operations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("redirection operations are not an array"))?;
    let mut resources = BTreeSet::new();
    resources.extend(descriptors.values().copied());
    for operation in operations {
        if operation["kind"] == "open" {
            resources.insert(
                operation["resource"]
                    .as_u64()
                    .ok_or_else(|| BridgeError::new("open resource is not a natural number"))?,
            );
        }
    }
    let resource_paths = resources
        .iter()
        .map(|resource| {
            let path = root.join(format!("resource-{resource}"));
            fs::File::create(&path).map_err(|error| {
                BridgeError::new(format!("cannot create redirection resource: {error}"))
            })?;
            Ok((*resource, path))
        })
        .collect::<BridgeResult<BTreeMap<_, _>>>()?;
    let mut shell = Shell::builder().cwd(root.to_path_buf()).env_clear().build();
    for (fd, resource) in &descriptors {
        let path = resource_paths
            .get(resource)
            .ok_or_else(|| BridgeError::new("initial descriptor resource has no file"))?;
        let source = format!("exec {fd}>{}", shell_quote(path));
        if evaluate_source(&mut shell, &source).is_err() {
            return Err(BridgeError::new("epsh could not initialize a real descriptor"));
        }
    }
    let permanent = request.input["permanent"].as_bool().unwrap_or(false);
    let mut setup_error = false;
    if permanent {
        for operation in operations {
            let source = redirection_command(operation, &resource_paths, root)?;
            if evaluate_source(&mut shell, &source).is_err() {
                setup_error = true;
                break;
            }
        }
    } else {
        let mut source = String::from(":");
        for operation in operations {
            source.push(' ');
            source.push_str(&redirection_fragment(operation, &resource_paths, root)?);
        }
        setup_error = evaluate_source(&mut shell, &source).is_err();
    }
    let mut probes: BTreeSet<u64> = descriptors.keys().copied().collect();
    for operation in operations {
        probes.insert(operation["target"].as_u64().unwrap_or(0));
        probes.insert(operation["copy"].as_u64().unwrap_or(0));
    }
    let identities = resource_paths
        .iter()
        .map(|(resource, path)| {
            let metadata = fs::metadata(path).map_err(|error| {
                BridgeError::new(format!("cannot inspect redirection resource: {error}"))
            })?;
            Ok((*resource, (metadata.dev(), metadata.ino())))
        })
        .collect::<BridgeResult<BTreeMap<_, _>>>()?;
    let mut observed = Vec::new();
    for fd in probes {
        if let Some(resource) = descriptor_resource(fd, &identities)? {
            observed.push(json!([fd, resource]));
        }
    }
    Ok(json!({
        "tag": "success",
        "value": {"descriptors": observed, "setup_error": setup_error},
    }))
    })
}

fn descriptor_resource(
    fd: u64,
    identities: &BTreeMap<u64, (u64, u64)>,
) -> BridgeResult<Option<u64>> {
    let path = PathBuf::from(format!("/dev/fd/{fd}"));
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BridgeError::new(format!("cannot inspect descriptor {fd}: {error}")));
        }
    };
    Ok(identities
        .iter()
        .find(|(_, identity)| **identity == (metadata.dev(), metadata.ino()))
        .map(|(resource, _)| *resource))
}

fn redirection_command(
    operation: &Value,
    resources: &BTreeMap<u64, PathBuf>,
    root: &Path,
) -> BridgeResult<String> {
    let fragment = redirection_fragment(operation, resources, root)?;
    Ok(format!("exec {fragment}"))
}

fn redirection_fragment(
    operation: &Value,
    resources: &BTreeMap<u64, PathBuf>,
    root: &Path,
) -> BridgeResult<String> {
    let target = operation["target"]
        .as_u64()
        .ok_or_else(|| BridgeError::new("redirection target is not a natural number"))?;
    let kind = operation["kind"]
        .as_str()
        .ok_or_else(|| BridgeError::new("redirection kind is not text"))?;
    let resource = operation["resource"]
        .as_u64()
        .ok_or_else(|| BridgeError::new("redirection resource is not a natural number"))?;
    match kind {
        "open" => {
            let path = resources
                .get(&resource)
                .ok_or_else(|| BridgeError::new("open redirection has no real resource"))?;
            Ok(format!("{target}>{}", shell_quote(path)))
        }
        "duplicate" => Ok(format!("{target}>&{resource}")),
        "close" => Ok(format!("{target}>&-")),
        "fail" => Ok(format!(
            "{target}>{}",
            shell_quote(&root.join("missing-parent").join("failure"))
        )),
        _ => Err(BridgeError::new(format!("unsupported redirection kind {kind:?}"))),
    }
}

fn evaluate_source(shell: &mut Shell, source: &str) -> epsh::error::Result<epsh::error::ExitStatus> {
    let program = Parser::new(source).parse()?;
    let mut status = epsh::error::ExitStatus::SUCCESS;
    for command in &program.commands {
        status = shell.eval_command(command)?;
    }
    Ok(status)
}

fn shell_quote(path: &Path) -> String {
    let bytes = ShellBytes::from_os_str(path.as_os_str()).to_shell_string();
    let mut quoted = String::from("'");
    for character in bytes.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn decode_bindings(value: &Value, context: &str) -> BridgeResult<Vec<(String, Vec<u8>)>> {
    let entries = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an array")))?;
    let mut names = BTreeSet::new();
    let mut decoded = Vec::with_capacity(entries.len());
    for entry in entries {
        let pair = entry
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| BridgeError::new(format!("{context} entry is not a pair")))?;
        let name = pair[0]
            .as_str()
            .ok_or_else(|| BridgeError::new(format!("{context} name is not text")))?
            .to_string();
        if !names.insert(name.clone()) {
            return Err(BridgeError::new(format!("{context} contains duplicate names")));
        }
        decoded.push((name, require_bytes(&pair[1], context)?));
    }
    Ok(decoded)
}

fn decode_environment(value: &Value, context: &str) -> BridgeResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let entries = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an array")))?;
    let mut names = BTreeSet::new();
    let mut decoded = Vec::with_capacity(entries.len());
    for entry in entries {
        let pair = entry
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| BridgeError::new(format!("{context} entry is not a pair")))?;
        let name = require_bytes(&pair[0], context)?;
        if !names.insert(name.clone()) {
            return Err(BridgeError::new(format!("{context} contains duplicate names")));
        }
        decoded.push((name, require_bytes(&pair[1], context)?));
    }
    Ok(decoded)
}

fn decode_descriptors(value: &Value) -> BridgeResult<BTreeMap<u64, u64>> {
    let entries = value
        .as_array()
        .ok_or_else(|| BridgeError::new("descriptors is not an array"))?;
    let mut decoded = BTreeMap::new();
    for entry in entries {
        let pair = entry
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| BridgeError::new("descriptor entry is not a pair"))?;
        let fd = require_fd(&pair[0], "descriptor number")?;
        let resource = require_fd(&pair[1], "descriptor resource")?;
        if !(MIN_DESCRIPTOR..=MAX_DESCRIPTOR).contains(&fd) || resource > 255 {
            return Err(BridgeError::new("descriptor is outside the generated subset"));
        }
        if decoded.insert(fd, resource).is_some() {
            return Err(BridgeError::new("descriptors contain duplicate numbers"));
        }
    }
    Ok(decoded)
}

fn require_fd(value: &Value, context: &str) -> BridgeResult<u64> {
    value
        .as_u64()
        .filter(|fd| *fd <= i32::MAX as u64)
        .ok_or_else(|| BridgeError::new(format!("{context} is outside the supported range")))
}

fn with_scratch<T>(run: impl FnOnce(&Path) -> BridgeResult<T>) -> BridgeResult<T> {
    let scratch = create_scratch()?;
    let result = run(&scratch);
    let cleanup = fs::remove_dir_all(&scratch)
        .map_err(|error| BridgeError::new(format!("cannot remove effects scratch: {error}")));
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(BridgeError::new(format!("{error}; {cleanup}"))),
    }
}

fn create_scratch() -> BridgeResult<PathBuf> {
    let manifest = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .map_err(|error| BridgeError::new(format!("cannot resolve effects workspace: {error}")))?;
    let work = manifest.join(".work");
    let tmp = work.join("tmp");
    let parent = tmp.join("semantic-bridge-effects");
    create_real_directory(&work)?;
    create_real_directory(&tmp)?;
    create_real_directory(&parent)?;
    for _ in 0..1024 {
        let counter = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{}-{counter}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(BridgeError::new(format!("cannot create effects scratch: {error}")));
            }
        }
    }
    Err(BridgeError::new("cannot allocate a unique effects scratch directory"))
}

fn create_real_directory(path: &Path) -> BridgeResult<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                BridgeError::new(format!("cannot inspect effects scratch directory: {error}"))
            })?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(BridgeError::new("effects scratch path is not a real directory"))
            }
        }
        Err(error) => Err(BridgeError::new(format!("cannot create effects scratch directory: {error}"))),
    }
}

fn scratch_path(root: &Path, namespace: &str, bytes: &[u8]) -> PathBuf {
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    root.join(namespace).join(format!("x{encoded}"))
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    let mut altered = outcome.clone();
    match altered["tag"].as_str() {
        Some("success") => match altered["value"].as_object_mut() {
            Some(value) => {
                value.insert("negative_control".into(), json!(true));
            }
            None => return Err(BridgeError::new("effects result value is not an object")),
        },
        Some("unsupported_domain") => altered["reason"] = json!("negative control"),
        Some("fuel_exhausted") => return Err(BridgeError::new("effects negative control needs an observed value")),
        _ => return Err(BridgeError::new("effects negative control received an unknown outcome")),
    }
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    validate_case(request)?;
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::<String>::new();
    match request.input["mode"].as_str().unwrap_or("") {
        "bytes" => {
            let bytes = require_bytes(&request.input["bytes"], "bytes input")?;
            for length in [bytes.len() / 2, 0] {
                let mut candidate = request.clone();
                candidate.input["bytes"] = json!(&bytes[..length]);
                add_candidate(&mut candidates, &mut seen, request, candidate)?;
            }
        }
        "environment" => {
            let assignments = request.input["assignments"].as_array().unwrap();
            for length in [assignments.len() / 2, 0] {
                let mut candidate = request.clone();
                candidate.input["assignments"] = Value::Array(assignments[..length].to_vec());
                add_candidate(&mut candidates, &mut seen, request, candidate)?;
            }
        }
        "cwd" => {
            let mut candidate = request.clone();
            candidate.input["success"] = json!(false);
            add_candidate(&mut candidates, &mut seen, request, candidate)?;
        }
        "redirection" => {
            let operations = request.input["operations"].as_array().unwrap();
            for length in [operations.len() / 2, operations.len().saturating_sub(1), 0] {
                let mut candidate = request.clone();
                candidate.input["operations"] = Value::Array(operations[..length].to_vec());
                add_candidate(&mut candidates, &mut seen, request, candidate)?;
            }
        }
        _ => return Err(BridgeError::new("effects shrink saw an unknown mode")),
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_request_initializes_unexported_values_without_exporting_them() {
        let mut request = generate(3, 0).expect("environment case");
        request.state["variables"] = json!([["BRIDGE_PRIVATE", [255, 128, 112]]]);
        let variables = environment_variables(&request).expect("initialized environment variables");
        assert_eq!(
            variables.get_bytes("BRIDGE_PRIVATE").map(ShellBytes::as_bytes),
            Some([255, 128, 112].as_slice()),
        );
        assert!(variables.env_for_command_os(&[]).iter().all(|(name, _)| name != "BRIDGE_PRIVATE"));
    }

    #[test]
    fn generated_effects_cases_stay_in_the_semantic_domain() {
        for index in 0..1000 {
            let request = generate(index, 0x5e6d_616e_7469_6373).expect("generated effects case");
            validate_case(&request).expect("valid effects case");
            assert_ne!(request.fuel, 0);
            if request.input["mode"] == "environment" {
                assert_eq!(
                    environment_domain_error(&request).expect("environment domain check"),
                    None
                );
            }
        }
    }
}

fn add_candidate(
    candidates: &mut Vec<Request>,
    seen: &mut BTreeSet<String>,
    original: &Request,
    candidate: Request,
) -> BridgeResult<()> {
    if candidate.input != original.input {
        let key = serde_json::to_string(&candidate.input)
            .map_err(|error| BridgeError::new(format!("cannot encode effects shrink candidate: {error}")))?;
        if seen.insert(key) {
            candidates.push(candidate);
        }
    }
    Ok(())
}
