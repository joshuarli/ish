use crate::semantic_bridge::arithmetic;
use crate::semantic_bridge::expansion;
use crate::semantic_bridge::interactive;
use crate::semantic_bridge::policy;
use crate::semantic_bridge::protocol::{Family, Request, validate_response};
use crate::semantic_bridge::{BridgeError, BridgeResult};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_SEED: u64 = 0x5e6d_616e_7469_6373;

struct Options {
    families: Vec<Family>,
    cases: usize,
    seed: u64,
    selected_id: Option<String>,
}

pub(crate) fn run(args: impl IntoIterator<Item = String>) -> BridgeResult<()> {
    let options = parse_args(args)?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = root.join("formal/.lake/build/bin/semanticBridge");
    if !binary.is_file() {
        return Err(BridgeError::new(format!(
            "Lean executable is missing at {}; build it with `cd formal && lake build semanticBridge` first",
            binary.display()
        )));
    }
    let tmpdir = root.join(".work/tmp");
    fs::create_dir_all(&tmpdir).map_err(|error| {
        BridgeError::new(format!("cannot create {}: {error}", tmpdir.display()))
    })?;
    verify_rust_protocol_boundary(&root)?;
    verify_protocol_boundary(&binary, &tmpdir, &root)?;
    let mut groups = Vec::new();
    for family in options.families.iter().copied() {
        let (mut cases, generated_count) = match options.selected_id.as_deref() {
            Some(id) => match load_regression_by_id(&root, family, id)? {
                Some(request) => (vec![request], 0),
                None => {
                    let cases = generate_cases(family, options.cases, options.seed, Some(id))?;
                    let count = cases.len();
                    (cases, count)
                }
            },
            None => {
                let cases = generate_cases(family, options.cases, options.seed, None)?;
                let count = cases.len();
                (cases, count)
            }
        };
        if options.selected_id.is_none() {
            cases.extend(load_regressions(&root, family)?);
        }
        for case in &cases {
            validate_case(case)?;
        }
        groups.push((family, cases, generated_count));
    }
    let requests: Vec<Request> = groups
        .iter()
        .flat_map(|(_, cases, _)| cases.iter().cloned())
        .collect();
    validate_unique_ids(&requests)?;
    let responses = run_lean(&binary, &tmpdir, &requests)?;
    if responses.len() != requests.len() {
        return Err(BridgeError::new(format!(
            "Lean returned {} responses for {} requests",
            responses.len(),
            requests.len()
        )));
    }
    if let Some(response) = responses.first() {
        verify_response_corruption(response.clone())?;
    }
    let mut offset = 0;
    for (family, cases, generated_count) in groups {
        let mut normalized = Vec::with_capacity(cases.len());
        for (index, request) in cases.iter().enumerate() {
            let actual = actual(request)?;
            let observed = validate_response(responses[offset + index].clone(), &request.id)?;
            let expected = normalize_actual(&actual, &request.id)?;
            if expected != observed {
                let minimized = minimize_mismatch(&binary, &tmpdir, family, request)?;
                let mut retained = minimized;
                retained.id = regression_filename(family, &retained.id);
                let retained_id = retained.id.clone();
                retain_regression(&root, family, &retained)?;
                return Err(BridgeError::new(format!(
                    "{} mismatch at {}; expected {}, Lean returned {}; minimized request saved to formal/fixtures/bridge-regressions/{}/{}.json",
                    family.name(),
                    request.id,
                    expected,
                    observed,
                    family.name(),
                    retained_id
                )));
            }
            normalized.push(expected);
        }
        if let (Some(request), Some(outcome)) = (cases.first(), normalized.first()) {
            let altered = negative_control(family, outcome)?;
            if altered == *outcome {
                return Err(BridgeError::new(format!(
                    "{} negative control did not mutate an observation",
                    family.name()
                )));
            }
            if altered == validate_response(responses[offset].clone(), &request.id)? {
                return Err(BridgeError::new(format!(
                    "{} negative control was not detected",
                    family.name()
                )));
            }
        }
        println!(
            "{}: {} generated, {} retained, negative control detected",
            family.name(),
            generated_count,
            cases.len() - generated_count,
        );
        offset += cases.len();
    }
    Ok(())
}

fn load_regressions(root: &Path, family: Family) -> BridgeResult<Vec<Request>> {
    let directory = root
        .join("formal/fixtures/bridge-regressions")
        .join(family.name());
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(BridgeError::new(format!("cannot read regression fixtures: {error}"))),
    };
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| BridgeError::new(format!("cannot read regression entry: {error}")))
        })
        .collect::<BridgeResult<Vec<_>>>()?;
    paths.sort();
    let mut requests = Vec::new();
    for path in paths.into_iter().filter(|path| path.extension().is_some_and(|extension| extension == "json")) {
        let bytes = fs::read(&path)
            .map_err(|error| BridgeError::new(format!("cannot read regression {}: {error}", path.display())))?;
        let json = serde_json::from_slice(&bytes)
            .map_err(|error| BridgeError::new(format!("regression {} is malformed: {error}", path.display())))?;
        let mut request = Request::from_json(json)?;
        if request.family != family {
            return Err(BridgeError::new(format!("regression {} has the wrong family", path.display())));
        }
        request.id = format!("{}-regression-{}", family.name(), request.id);
        requests.push(request);
    }
    Ok(requests)
}

fn load_regression_by_id(root: &Path, family: Family, id: &str) -> BridgeResult<Option<Request>> {
    selected_index(id, family)?;
    let path = root
        .join("formal/fixtures/bridge-regressions")
        .join(family.name())
        .join(format!("{id}.json"));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BridgeError::new(format!("cannot read regression {}: {error}", path.display()))),
    };
    let json = serde_json::from_slice(&bytes)
        .map_err(|error| BridgeError::new(format!("regression {} is malformed: {error}", path.display())))?;
    let request = Request::from_json(json)?;
    if request.family != family || request.id != id {
        return Err(BridgeError::new(format!("regression {} has the wrong family or ID", path.display())));
    }
    Ok(Some(request))
}

fn regression_filename(family: Family, id: &str) -> String {
    id.strip_prefix(&format!("{}-regression-", family.name()))
        .unwrap_or(id)
        .to_string()
}

fn verify_response_corruption(response: Value) -> BridgeResult<()> {
    let mut corrupted = response;
    corrupted["version"] = json!(2);
    if validate_response(corrupted, "corruption-probe").is_ok() {
        return Err(BridgeError::new("bridge accepted a corrupted Lean response"));
    }
    let mut malformed_trace = json!({
        "version": 1,
        "id": "corruption-probe",
        "outcome": {
            "tag": "semantic_error",
            "error": {"kind": "arithmetic", "message": "ignored diagnostic"},
            "state": crate::semantic_bridge::protocol::empty_state(),
            "trace": [{"tag": "unknown"}],
        },
    });
    if validate_response(malformed_trace.clone(), "corruption-probe").is_ok() {
        return Err(BridgeError::new("bridge accepted an unknown Lean trace event"));
    }
    malformed_trace["outcome"]["trace"] = json!([]);
    if validate_response(malformed_trace, "wrong-id").is_ok() {
        return Err(BridgeError::new("bridge accepted a Lean response with the wrong ID"));
    }
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> BridgeResult<Options> {
    let mut family_arg = "all".to_string();
    let mut cases = 1000usize;
    let mut seed = DEFAULT_SEED;
    let mut selected_id = None;
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--family" => {
                family_arg = args
                    .next()
                    .ok_or_else(|| BridgeError::new("--family requires a value"))?;
            }
            "--cases" => {
                let value = args
                    .next()
                    .ok_or_else(|| BridgeError::new("--cases requires a value"))?;
                cases = value
                    .parse()
                    .map_err(|_| BridgeError::new("--cases must be a natural number"))?;
                if cases == 0 {
                    return Err(BridgeError::new("--cases must be greater than zero"));
                }
            }
            "--seed" => {
                let value = args
                    .next()
                    .ok_or_else(|| BridgeError::new("--seed requires a value"))?;
                seed = value
                    .parse()
                    .map_err(|_| BridgeError::new("--seed must be an unsigned integer"))?;
            }
            "--id" => {
                selected_id = Some(
                    args.next()
                        .ok_or_else(|| BridgeError::new("--id requires a value"))?,
                );
            }
            "--help" | "-h" => {
                println!("usage: cargo run --example semantic_bridge -p ish -- --family <name|all> --cases <count> [--seed <u64>] [--id <family-NNNNN>]");
                std::process::exit(0);
            }
            _ => return Err(BridgeError::new(format!("unknown option {argument:?}"))),
        }
    }
    let families = if family_arg == "all" {
        Family::ALL.to_vec()
    } else {
        vec![Family::parse(&family_arg)?]
    };
    if let Some(id) = &selected_id {
        let family = family_from_id(id)?;
        if families.len() != 1 || families[0] != family {
            return Err(BridgeError::new("--id must belong to the selected single family"));
        }
    }
    Ok(Options {
        families,
        cases,
        seed,
        selected_id,
    })
}

fn family_from_id(id: &str) -> BridgeResult<Family> {
    for family in Family::ALL {
        if id.starts_with(&format!("{}-", family.name())) {
            return Ok(family);
        }
    }
    Err(BridgeError::new(format!("case ID {id:?} has no known family prefix")))
}

fn selected_index(id: &str, family: Family) -> BridgeResult<usize> {
    let prefix = format!("{}-", family.name());
    let index = id
        .strip_prefix(&prefix)
        .ok_or_else(|| BridgeError::new("case ID does not match selected family"))?;
    if index.len() != 5 || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BridgeError::new("case ID must end in five decimal digits"));
    }
    index
        .parse()
        .map_err(|_| BridgeError::new("case ID index is invalid"))
}

fn generate_cases(
    family: Family,
    count: usize,
    seed: u64,
    selected_id: Option<&str>,
) -> BridgeResult<Vec<Request>> {
    if let Some(id) = selected_id {
        let index = selected_index(id, family)?;
        let mut cases = vec![generate(family, index, seed)?];
        cases[0].id = id.to_string();
        return Ok(cases);
    }
    (0..count)
        .map(|index| generate(family, index, seed))
        .collect()
}

fn generate(family: Family, index: usize, seed: u64) -> BridgeResult<Request> {
    match family {
        Family::Arithmetic => Ok(arithmetic::generate(index, seed)),
        Family::Interactive => Ok(interactive::generate(index, seed)),
        Family::Policy => policy::generate(index, seed),
        Family::Expansion => expansion::generate(index, seed),
        Family::Evaluation => Err(BridgeError::new("evaluation adapter is not integrated")),
        Family::Effects => Err(BridgeError::new("effects adapter is not integrated")),
    }
}

fn validate_case(request: &Request) -> BridgeResult<()> {
    match request.family {
        Family::Arithmetic => arithmetic::validate_case(request),
        Family::Interactive => interactive::validate_case(request),
        Family::Policy => policy::validate_case(request),
        Family::Expansion => expansion::validate_case(request),
        Family::Evaluation | Family::Effects => Err(BridgeError::new("selected family adapter is not integrated")),
    }
}

fn actual(request: &Request) -> BridgeResult<Value> {
    match request.family {
        Family::Arithmetic => arithmetic::actual(request),
        Family::Interactive => interactive::actual(request),
        Family::Policy => policy::actual(request),
        Family::Expansion => expansion::actual(request),
        Family::Evaluation | Family::Effects => Err(BridgeError::new("selected family adapter is not integrated")),
    }
}

fn negative_control(family: Family, outcome: &Value) -> BridgeResult<Value> {
    match family {
        Family::Arithmetic => arithmetic::negative_control(outcome),
        Family::Interactive => interactive::negative_control(outcome),
        Family::Policy => policy::negative_control(outcome),
        Family::Expansion => expansion::negative_control(outcome),
        Family::Evaluation | Family::Effects => Err(BridgeError::new("selected family negative control is not integrated")),
    }
}

fn normalize_actual(outcome: &Value, id: &str) -> BridgeResult<Value> {
    validate_response(
        json!({"version": 1, "id": id, "outcome": outcome}),
        id,
    )
}

fn run_lean(binary: &Path, tmpdir: &Path, requests: &[Request]) -> BridgeResult<Vec<Value>> {
    let mut child = Command::new(binary)
        .current_dir(lean_project_dir(binary)?)
        .env("TMPDIR", tmpdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| BridgeError::new(format!("cannot start Lean bridge: {error}")))?;
    let mut input = Vec::new();
    for request in requests {
        serde_json::to_writer(&mut input, &request.to_json())
            .map_err(|error| BridgeError::new(format!("cannot encode request: {error}")))?;
        input.push(b'\n');
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| BridgeError::new("Lean bridge stdin is unavailable"))?;
    let writer = std::thread::spawn(move || {
        let mut stdin = stdin;
        stdin.write_all(&input)
    });
    let output = child
        .wait_with_output()
        .map_err(|error| BridgeError::new(format!("cannot wait for Lean bridge: {error}")))?;
    writer
        .join()
        .map_err(|_| BridgeError::new("Lean request writer panicked"))?
        .map_err(|error| BridgeError::new(format!("cannot write request: {error}")))?;
    if !output.status.success() {
        return Err(BridgeError::new(format!(
            "Lean bridge exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| BridgeError::new("Lean bridge output is not UTF-8"))?;
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| BridgeError::new(format!("Lean response is malformed: {error}")))
        })
        .collect()
}

fn verify_protocol_boundary(binary: &Path, tmpdir: &Path, root: &Path) -> BridgeResult<()> {
    let invalid = fs::read_to_string(root.join("formal/fixtures/protocol-invalid.jsonl"))
        .map_err(|error| BridgeError::new(format!("cannot read protocol rejection goldens: {error}")))?;
    let valid = fs::read_to_string(root.join("formal/fixtures/protocol-v1.jsonl"))
        .map_err(|error| BridgeError::new(format!("cannot read protocol acceptance goldens: {error}")))?;
    let mut input = String::new();
    let expected = [
        "unsupported_version",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
        "malformed",
    ];
    let lines: Vec<&str> = invalid.lines().collect();
    if lines.len() != expected.len() {
        return Err(BridgeError::new("protocol-invalid fixture count changed"));
    }
    for line in &lines {
        input.push_str(line);
        input.push('\n');
    }
    let valid_lines: Vec<&str> = valid.lines().collect();
    if valid_lines.len() != Family::ALL.len() {
        return Err(BridgeError::new("protocol-v1 fixture count changed"));
    }
    for line in &valid_lines {
        input.push_str(line);
        input.push('\n');
    }
    input.push_str(valid_lines[0]);
    input.push('\n');
    let outputs = run_lean_text(binary, tmpdir, root.join("formal"), &input)?;
    if outputs.len() != expected.len() + valid_lines.len() + 1 {
        return Err(BridgeError::new("Lean protocol rejection response count changed"));
    }
    for (line_index, (response, expected_tag)) in outputs.iter().zip(expected).enumerate() {
        validate_boundary_error(response, expected_tag, None)?;
        if response["error"]["tag"] != expected_tag {
            return Err(BridgeError::new(format!(
                "protocol rejection golden {} returned {:?}, expected {expected_tag}",
                line_index + 1,
                response["error"]["tag"]
            )));
        }
    }
    for (line, response) in valid_lines
        .iter()
        .zip(&outputs[expected.len()..expected.len() + valid_lines.len()])
    {
        let request: Value = serde_json::from_str(line)
            .map_err(|error| BridgeError::new(format!("protocol-v1 fixture is malformed: {error}")))?;
        let normalized = validate_response(response.clone(), request["id"].as_str().unwrap_or(""))?;
        if normalized["tag"] != "unsupported_domain" {
            return Err(BridgeError::new("valid protocol golden did not reach family validation"));
        }
    }
    validate_boundary_error(
        &outputs[expected.len() + valid_lines.len()],
        "duplicate_id",
        Some("golden-policy"),
    )?;
    Ok(())
}

fn validate_boundary_error(response: &Value, tag: &str, id: Option<&str>) -> BridgeResult<()> {
    crate::semantic_bridge::protocol::exact_object(response, &["version", "error"], "boundary error response")?;
    if response["version"] != 1 {
        return Err(BridgeError::new("boundary error has an unsupported response version"));
    }
    let fields = match tag {
        "malformed" => &["tag", "message"][..],
        "unsupported_version" => &["tag", "version"][..],
        "duplicate_id" => &["tag", "id"][..],
        _ => return Err(BridgeError::new(format!("unknown expected boundary tag {tag:?}"))),
    };
    crate::semantic_bridge::protocol::exact_object(&response["error"], fields, "boundary error payload")?;
    if response["error"]["tag"] != tag {
        return Err(BridgeError::new(format!("boundary error tag is not {tag:?}")));
    }
    match tag {
        "malformed" if response["error"]["message"].is_string() => {}
        "unsupported_version" if response["error"]["version"] == 2 => {}
        "duplicate_id" if response["error"]["id"].as_str() == id => {}
        _ => return Err(BridgeError::new(format!("boundary {tag:?} payload is invalid"))),
    }
    Ok(())
}

fn verify_rust_protocol_boundary(root: &Path) -> BridgeResult<()> {
    let invalid = fs::read_to_string(root.join("formal/fixtures/protocol-invalid.jsonl"))
        .map_err(|error| BridgeError::new(format!("cannot read protocol rejection goldens: {error}")))?;
    let valid = fs::read_to_string(root.join("formal/fixtures/protocol-v1.jsonl"))
        .map_err(|error| BridgeError::new(format!("cannot read protocol acceptance goldens: {error}")))?;
    let invalid_lines: Vec<&str> = invalid.lines().collect();
    if invalid_lines.len() != 12 {
        return Err(BridgeError::new("protocol-invalid fixture count changed"));
    }
    for (index, line) in invalid_lines.iter().enumerate() {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) if index != 0 => continue,
            Err(error) => {
                return Err(BridgeError::new(format!(
                    "unsupported-version golden is malformed: {error}"
                )));
            }
        };
        if index == 0 && value["version"] != 2 {
            return Err(BridgeError::new("unsupported-version golden changed"));
        }
        if index != 0 && value["version"] != 1 {
            return Err(BridgeError::new(format!(
                "malformed protocol golden {} has the wrong version",
                index + 1
            )));
        }
        if Request::from_json(value).is_ok() {
            return Err(BridgeError::new(format!(
                "Rust accepted invalid protocol golden {}",
                index + 1
            )));
        }
    }
    let valid_lines: Vec<&str> = valid.lines().collect();
    if valid_lines.len() != Family::ALL.len() {
        return Err(BridgeError::new("protocol-v1 fixture count changed"));
    }
    let mut requests = Vec::with_capacity(valid_lines.len());
    for (line, expected_family) in valid_lines.iter().zip(Family::ALL) {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| BridgeError::new(format!("protocol-v1 fixture is malformed: {error}")))?;
        let request = Request::from_json(value)?;
        if request.family != expected_family {
            return Err(BridgeError::new("protocol-v1 family order changed"));
        }
        requests.push(request);
    }
    validate_unique_ids(&requests)?;
    let mut duplicate_requests = requests;
    let duplicate = duplicate_requests[0].clone();
    duplicate_requests.push(duplicate);
    if validate_unique_ids(&duplicate_requests).is_ok() {
        return Err(BridgeError::new("Rust accepted a duplicate protocol ID"));
    }
    Ok(())
}

fn validate_unique_ids(requests: &[Request]) -> BridgeResult<()> {
    let mut ids = HashSet::new();
    for request in requests {
        if !ids.insert(request.id.as_str()) {
            return Err(BridgeError::new(format!(
                "duplicate request ID {:?}",
                request.id
            )));
        }
    }
    Ok(())
}

fn run_lean_text(
    binary: &Path,
    tmpdir: &Path,
    project_dir: PathBuf,
    input: &str,
) -> BridgeResult<Vec<Value>> {
    let mut child = Command::new(binary)
        .current_dir(project_dir)
        .env("TMPDIR", tmpdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| BridgeError::new(format!("cannot run Lean protocol check: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| BridgeError::new("Lean protocol stdin is unavailable"))?
        .write_all(input.as_bytes())
        .map_err(|error| BridgeError::new(format!("cannot write protocol goldens: {error}")))?;
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| BridgeError::new(format!("cannot wait for protocol check: {error}")))?;
    if !output.status.success() {
        return Err(BridgeError::new(format!(
            "Lean protocol check exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|_| BridgeError::new("Lean protocol output is not UTF-8"))?
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| BridgeError::new(format!("Lean protocol response is malformed: {error}")))
        })
        .collect()
}

fn minimize_mismatch(
    binary: &Path,
    tmpdir: &Path,
    family: Family,
    request: &Request,
) -> BridgeResult<Request> {
    let mut candidate = request.clone();
    loop {
        let mut changed = false;
        for reduced in shrink(family, &candidate)? {
            let responses = run_lean(binary, tmpdir, &[reduced.clone()])?;
            let Some(response) = responses.into_iter().next() else {
                continue;
            };
            if normalize_actual(&actual(&reduced)?, &reduced.id)?
                == validate_response(response, &reduced.id)?
            {
                continue;
            }
            candidate = reduced;
            changed = true;
            break;
        }
        if !changed {
            break;
        }
    }
    let responses = run_lean(binary, tmpdir, &[candidate.clone()])?;
    let response = responses
        .into_iter()
        .next()
        .ok_or_else(|| BridgeError::new("Lean returned no minimized response"))?;
    if normalize_actual(&actual(&candidate)?, &candidate.id)?
        == validate_response(response, &candidate.id)?
    {
        return Err(BridgeError::new("mismatch minimizer lost the original discrepancy"));
    }
    Ok(candidate)
}

fn shrink(family: Family, request: &Request) -> BridgeResult<Vec<Request>> {
    match family {
        Family::Arithmetic => arithmetic::shrink(request),
        Family::Interactive => interactive::shrink(request),
        Family::Policy => policy::shrink(request),
        Family::Expansion => expansion::shrink(request),
        Family::Evaluation | Family::Effects => Err(BridgeError::new("selected family shrinker is not integrated")),
    }
}

fn lean_project_dir(binary: &Path) -> BridgeResult<&Path> {
    binary
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| BridgeError::new("Lean binary has no project directory"))
}

fn retain_regression(root: &Path, family: Family, request: &Request) -> BridgeResult<()> {
    let directory = root
        .join("formal/fixtures/bridge-regressions")
        .join(family.name());
    fs::create_dir_all(&directory).map_err(|error| {
        BridgeError::new(format!("cannot create regression directory: {error}"))
    })?;
    let path = directory.join(format!("{}.json", regression_filename(family, &request.id)));
    let bytes = serde_json::to_vec_pretty(&request.to_json())
        .map_err(|error| BridgeError::new(format!("cannot encode regression fixture: {error}")))?;
    fs::write(&path, bytes)
        .map_err(|error| BridgeError::new(format!("cannot retain {}: {error}", path.display())))
}
