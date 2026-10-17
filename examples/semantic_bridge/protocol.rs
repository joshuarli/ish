use crate::semantic_bridge::{BridgeError, BridgeResult};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Family {
    Arithmetic,
    Interactive,
    Policy,
    Expansion,
    Evaluation,
    Effects,
}

impl Family {
    pub(crate) const ALL: [Self; 6] = [
        Self::Policy,
        Self::Expansion,
        Self::Arithmetic,
        Self::Evaluation,
        Self::Effects,
        Self::Interactive,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Arithmetic => "arithmetic",
            Self::Interactive => "interactive",
            Self::Policy => "policy",
            Self::Expansion => "expansion",
            Self::Evaluation => "evaluation",
            Self::Effects => "effects",
        }
    }

    pub(crate) fn parse(value: &str) -> BridgeResult<Self> {
        match value {
            "arithmetic" => Ok(Self::Arithmetic),
            "interactive" => Ok(Self::Interactive),
            "policy" => Ok(Self::Policy),
            "expansion" => Ok(Self::Expansion),
            "evaluation" => Ok(Self::Evaluation),
            "effects" => Ok(Self::Effects),
            _ => Err(BridgeError::new(format!("unknown family {value:?}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) id: String,
    pub(crate) family: Family,
    pub(crate) source: Vec<u8>,
    pub(crate) state: Value,
    pub(crate) fuel: u64,
    pub(crate) input: Value,
}

impl Request {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "id": self.id,
            "family": self.family.name(),
            "source": self.source,
            "state": self.state,
            "fuel": self.fuel,
            "input": self.input,
        })
    }

    pub(crate) fn from_json(value: Value) -> BridgeResult<Self> {
        exact_object(
            &value,
            &["version", "id", "family", "source", "state", "fuel", "input"],
            "request",
        )?;
        if value["version"] != 1 {
            return Err(BridgeError::new("request has an unsupported version"));
        }
        let id = value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| BridgeError::new("request ID is missing or empty"))?
            .to_string();
        let family = Family::parse(
            value["family"]
                .as_str()
                .ok_or_else(|| BridgeError::new("request family is not text"))?,
        )?;
        let source = require_bytes(&value["source"], "request source")?;
        validate_state(&value["state"])?;
        let fuel = value["fuel"]
            .as_u64()
            .ok_or_else(|| BridgeError::new("request fuel is not a natural number"))?;
        if !value["input"].is_object() {
            return Err(BridgeError::new("request input is not an object"));
        }
        Ok(Self {
            id,
            family,
            source,
            state: value["state"].clone(),
            fuel,
            input: value["input"].clone(),
        })
    }
}

pub(crate) fn empty_state() -> Value {
    json!({
        "variables": [],
        "exported": [],
        "inherited": [],
        "cwd": [],
        "positional": [],
        "last_status": 0,
    })
}

pub(crate) fn exact_object(value: &Value, fields: &[&str], context: &str) -> BridgeResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an object")))?;
    if object.len() != fields.len() || fields.iter().any(|field| !object.contains_key(*field)) {
        return Err(BridgeError::new(format!("{context} has incorrect fields")));
    }
    Ok(())
}

pub(crate) fn require_bytes(value: &Value, context: &str) -> BridgeResult<Vec<u8>> {
    let array = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not a byte array")))?;
    array
        .iter()
        .map(|item| {
            item.as_u64()
                .filter(|byte| *byte <= 255)
                .map(|byte| byte as u8)
                .ok_or_else(|| BridgeError::new(format!("{context} contains a non-byte")))
        })
        .collect()
}

pub(crate) fn validate_response(value: Value, expected_id: &str) -> BridgeResult<Value> {
    exact_object(&value, &["version", "id", "outcome"], "response")?;
    if value["version"] != 1 {
        return Err(BridgeError::new("response has an unsupported version"));
    }
    if value["id"].as_str() != Some(expected_id) {
        return Err(BridgeError::new("response id does not match request"));
    }
    let outcome = &value["outcome"];
    let tag = outcome["tag"]
        .as_str()
        .ok_or_else(|| BridgeError::new("response outcome has no tag"))?;
    match tag {
        "success" => exact_object(outcome, &["tag", "value"], "success outcome")?,
        "semantic_error" => {
            exact_object(outcome, &["tag", "error", "state", "trace"], "semantic error")?;
            validate_error(&outcome["error"])?;
            validate_state(&outcome["state"])?;
            validate_trace(&outcome["trace"])?;
        }
        "unsupported_domain" => {
            exact_object(outcome, &["tag", "reason"], "unsupported-domain outcome")?;
            if !outcome["reason"].is_string() {
                return Err(BridgeError::new("unsupported-domain reason is not text"));
            }
        }
        "fuel_exhausted" => exact_object(outcome, &["tag"], "fuel-exhausted outcome")?,
        _ => return Err(BridgeError::new(format!("unknown response outcome {tag:?}"))),
    }
    let mut normalized = outcome.clone();
    if normalized["tag"] == "semantic_error" {
        if let Some(error) = normalized["error"].as_object_mut() {
            error.remove("message");
        }
    }
    normalize_maps(&mut normalized)?;
    Ok(normalized)
}

fn normalize_maps(value: &mut Value) -> BridgeResult<()> {
    match value {
        Value::Object(object) => {
            for key in ["variables", "exported"] {
                if let Some(entries) = object.get_mut(key) {
                    validate_bindings(entries, key)?;
                    entries
                        .as_array_mut()
                        .expect("validated binding array")
                        .sort_by(|left, right| {
                            left[0]
                                .as_str()
                                .expect("validated binding name")
                                .cmp(right[0].as_str().expect("validated binding name"))
                        });
                }
            }
            for key in ["inherited", "environment"] {
                if let Some(entries) = object.get_mut(key) {
                    validate_pairs(entries, key)?;
                    entries
                        .as_array_mut()
                        .expect("validated byte-pair array")
                        .sort_by(|left, right| byte_key(&left[0]).cmp(&byte_key(&right[0])));
                }
            }
            if let Some(entries) = object.get_mut("descriptors") {
                validate_descriptors(entries)?;
                entries
                    .as_array_mut()
                    .expect("validated descriptor array")
                    .sort_by_key(|entry| entry[0].as_u64().expect("validated descriptor key"));
            }
            for child in object.values_mut() {
                normalize_maps(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_maps(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn byte_key(value: &Value) -> Vec<u8> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .map(|byte| byte as u8)
        .collect()
}

fn validate_descriptors(value: &Value) -> BridgeResult<()> {
    let entries = value
        .as_array()
        .ok_or_else(|| BridgeError::new("descriptor map is not an array"))?;
    let mut descriptors = std::collections::HashSet::new();
    for entry in entries {
        let pair = entry
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| BridgeError::new("descriptor entry is not a pair"))?;
        let descriptor = pair[0]
            .as_u64()
            .ok_or_else(|| BridgeError::new("descriptor key is not a natural number"))?;
        if !descriptors.insert(descriptor) {
            return Err(BridgeError::new("descriptor map has duplicate keys"));
        }
        if pair[1].as_u64().is_none() {
            return Err(BridgeError::new("descriptor value is not a natural number"));
        }
    }
    Ok(())
}

fn validate_error(value: &Value) -> BridgeResult<()> {
    let kind = value["kind"]
        .as_str()
        .ok_or_else(|| BridgeError::new("semantic error has no kind"))?;
    let keys = match kind {
        "syntax" | "arithmetic" | "expansion" | "environment" => {
            &["kind", "message"][..]
        }
        "policy" => &["kind", "target"][..],
        "redirection" | "restoration" => &["kind", "fd", "message"][..],
        _ => return Err(BridgeError::new(format!("unknown semantic error kind {kind:?}"))),
    };
    exact_object(value, keys, "semantic error payload")?;
    if value.get("message").is_some_and(|message| !message.is_string()) {
        return Err(BridgeError::new("semantic error diagnostic is not text"));
    }
    if kind == "policy" {
        require_bytes(&value["target"], "policy target")?;
    }
    if matches!(kind, "redirection" | "restoration") && value["fd"].as_u64().is_none() {
        return Err(BridgeError::new("descriptor error has no natural fd"));
    }
    Ok(())
}

fn validate_state(value: &Value) -> BridgeResult<()> {
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
        "shell state",
    )?;
    for field in ["cwd"] {
        require_bytes(&value[field], field)?;
    }
    let status = value["last_status"]
        .as_u64()
        .filter(|status| *status < 256)
        .ok_or_else(|| BridgeError::new("shell status is outside 0..255"))?;
    let _ = status;
    validate_bindings(&value["variables"], "variables")?;
    validate_bindings(&value["exported"], "exported")?;
    validate_pairs(&value["inherited"], "inherited")?;
    let positional = value["positional"]
        .as_array()
        .ok_or_else(|| BridgeError::new("positional state is not an array"))?;
    for item in positional {
        require_bytes(item, "positional argument")?;
    }
    Ok(())
}

fn validate_trace(value: &Value) -> BridgeResult<()> {
    let events = value
        .as_array()
        .ok_or_else(|| BridgeError::new("semantic error trace is not an array"))?;
    for event in events {
        let tag = event["tag"]
            .as_str()
            .ok_or_else(|| BridgeError::new("trace event has no tag"))?;
        match tag {
            "invoke" => {
                exact_object(event, &["tag", "argv", "environment", "cwd"], "invoke trace event")?;
                let argv = event["argv"]
                    .as_array()
                    .ok_or_else(|| BridgeError::new("invoke argv is not an array"))?;
                for bytes in argv {
                    require_bytes(bytes, "invoke argv")?;
                }
                validate_pairs(&event["environment"], "invoke environment")?;
                require_bytes(&event["cwd"], "invoke cwd")?;
            }
            "read" | "write" => {
                exact_object(event, &["tag", "fd", "bytes"], "I/O trace event")?;
                require_fd(&event["fd"])?;
                require_bytes(&event["bytes"], "I/O trace bytes")?;
            }
            "open" => {
                exact_object(event, &["tag", "fd", "path", "mode"], "open trace event")?;
                require_fd(&event["fd"])?;
                require_bytes(&event["path"], "open trace path")?;
                if !matches!(event["mode"].as_str(), Some("input" | "output" | "append" | "read_write" | "clobber")) {
                    return Err(BridgeError::new("open trace mode is invalid"));
                }
            }
            "duplicate" => {
                exact_object(event, &["tag", "source", "destination"], "duplicate trace event")?;
                require_fd(&event["source"])?;
                require_fd(&event["destination"])?;
            }
            "close" => {
                exact_object(event, &["tag", "fd"], "close trace event")?;
                require_fd(&event["fd"])?;
            }
            "change_directory" => {
                exact_object(event, &["tag", "path"], "directory trace event")?;
                require_bytes(&event["path"], "directory trace path")?;
            }
            _ => return Err(BridgeError::new(format!("unknown trace event {tag:?}"))),
        }
    }
    Ok(())
}

fn require_fd(value: &Value) -> BridgeResult<()> {
    value
        .as_u64()
        .filter(|fd| *fd <= i32::MAX as u64)
        .ok_or_else(|| BridgeError::new("trace descriptor is outside the supported range"))?;
    Ok(())
}

fn validate_bindings(value: &Value, context: &str) -> BridgeResult<()> {
    let bindings = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an array")))?;
    let mut names = std::collections::HashSet::new();
    for binding in bindings {
        if binding.as_array().is_none_or(|pair| pair.len() != 2) {
            return Err(BridgeError::new(format!("{context} entry is not a pair")));
        }
        let name = binding[0]
            .as_str()
            .ok_or_else(|| BridgeError::new(format!("{context} name is not text")))?;
        if !names.insert(name) {
            return Err(BridgeError::new(format!("{context} has duplicate names")));
        }
        require_bytes(&binding[1], context)?;
    }
    Ok(())
}

fn validate_pairs(value: &Value, context: &str) -> BridgeResult<()> {
    let pairs = value
        .as_array()
        .ok_or_else(|| BridgeError::new(format!("{context} is not an array")))?;
    let mut names = std::collections::HashSet::new();
    for pair in pairs {
        if pair.as_array().is_none_or(|pair| pair.len() != 2) {
            return Err(BridgeError::new(format!("{context} entry is not a pair")));
        }
        let name = require_bytes(&pair[0], context)?;
        if !names.insert(name) {
            return Err(BridgeError::new(format!("{context} has duplicate names")));
        }
        require_bytes(&pair[1], context)?;
    }
    Ok(())
}

