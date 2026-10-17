use crate::semantic_bridge::protocol::{Family, Request, empty_state, require_bytes};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use ish::line::LineBuffer;
use serde_json::{Value, json};

pub(crate) fn generate(index: usize, seed: u64) -> Request {
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0xd6e8feb86659fd93));
    let sources = ["", "alpha beta", "café λ", "漢字 x", "éx", "a\u{3000}b"];
    let source = sources[index % sources.len()];
    let cursor = match index % 5 {
        0 => 0,
        1 => source.len(),
        2 if source == "café λ" => 4,
        3 if source == "漢字 x" => 2,
        _ => random.below(source.len() as u64 + 4) as usize,
    };
    let mut operations = Vec::new();
    let choices = [
        "insert_char",
        "insert_str",
        "delete_back",
        "delete_forward",
        "kill_to_start",
        "kill_to_end",
        "kill_word_back",
        "kill_word_forward",
        "yank",
        "move_left",
        "move_right",
        "move_home",
        "move_end",
        "move_word_left",
        "move_word_right",
    ];
    let count = if index < 8 { 1 + index % 4 } else { random.below(24) as usize };
    for operation_index in 0..count {
        let operation = if index < 8 {
            [
                "move_home",
                "kill_word_forward",
                "move_word_right",
                "kill_to_start",
                "yank",
                "insert_str",
                "delete_back",
                "move_end",
            ][(index + operation_index) % 8]
        } else {
            choices[random.below(choices.len() as u64) as usize]
        };
        match operation {
            "insert_char" => {
                let text = [b"x".as_slice(), "λ".as_bytes(), "漢".as_bytes()][random.below(3) as usize];
                operations.push(json!({"op": operation, "text": text}));
            }
            "insert_str" => {
                let text = [b" ".as_slice(), b"tail", " café".as_bytes()][random.below(3) as usize];
                operations.push(json!({"op": operation, "text": text}));
            }
            _ => operations.push(json!({"op": operation})),
        }
    }
    Request {
        id: format!("interactive-{index:05}"),
        family: Family::Interactive,
        source: source.as_bytes().to_vec(),
        state: empty_state(),
        fuel: 128,
        input: json!({"cursor": cursor, "operations": operations}),
    }
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("interactive source is not UTF-8"))?;
    let cursor = request.input["cursor"]
        .as_u64()
        .ok_or_else(|| BridgeError::new("interactive cursor is not a natural number"))?
        as usize;
    let operations = request.input["operations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("interactive operations are not an array"))?;
    let mut line = LineBuffer::new();
    line.set_with_cursor(source, cursor);
    for operation in operations {
        let name = operation["op"]
            .as_str()
            .ok_or_else(|| BridgeError::new("interactive operation has no tag"))?;
        match name {
            "insert_char" => {
                let bytes = require_bytes(&operation["text"], "insert_char text")?;
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| BridgeError::new("insert_char text is not UTF-8"))?;
                let mut chars = value.chars();
                let character = chars
                    .next()
                    .ok_or_else(|| BridgeError::new("insert_char text is empty"))?;
                if chars.next().is_some() {
                    return Err(BridgeError::new("insert_char text has multiple characters"));
                }
                line.insert_char(character);
            }
            "insert_str" => {
                let bytes = require_bytes(&operation["text"], "insert_str text")?;
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| BridgeError::new("insert_str text is not UTF-8"))?;
                line.insert_str(value);
            }
            "delete_back" => {
                line.delete_back();
            }
            "delete_forward" => {
                line.delete_forward();
            }
            "kill_to_start" => line.kill_to_start(),
            "kill_to_end" => line.kill_to_end(),
            "kill_word_back" => line.kill_word_back(),
            "kill_word_forward" => line.kill_word_forward(),
            "yank" => line.yank(),
            "move_left" => {
                line.move_left();
            }
            "move_right" => {
                line.move_right();
            }
            "move_home" => line.move_home(),
            "move_end" => line.move_end(),
            "move_word_left" => line.move_word_left(),
            "move_word_right" => line.move_word_right(),
            _ => return Err(BridgeError::new(format!("unsupported interactive operation {name:?}"))),
        }
    }
    Ok(json!({
        "tag": "success",
        "value": {"text": line.text().as_bytes(), "cursor": line.cursor()},
    }))
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    let mut altered = outcome.clone();
    if altered["tag"] != "success" {
        return Err(BridgeError::new("interactive negative control needs success"));
    }
    let text = altered["value"]["text"]
        .as_array_mut()
        .ok_or_else(|| BridgeError::new("interactive result text is malformed"))?;
    text.push(json!(0));
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    let operations = request.input["operations"]
        .as_array()
        .ok_or_else(|| BridgeError::new("interactive operations are not an array"))?;
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for length in [operations.len() / 2, operations.len().saturating_sub(1), 0] {
        let mut candidate = request.clone();
        candidate.input["operations"] = Value::Array(operations[..length].to_vec());
        if seen.insert(candidate.input.clone()) {
            candidates.push(candidate);
        }
    }
    if !request.source.is_empty() {
        let mut candidate = request.clone();
        candidate.source.clear();
        candidate.input["cursor"] = json!(0);
        if seen.insert(candidate.input.clone()) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    if request.source.len() > 256 {
        return Err(BridgeError::new("generated interactive source exceeds 256 bytes"));
    }
    if request.input["operations"].as_array().is_none_or(|operations| operations.len() > 64) {
        return Err(BridgeError::new("generated interactive operation count exceeds 64"));
    }
    Ok(())
}
