use crate::semantic_bridge::protocol::{Family, Request};
use crate::semantic_bridge::random::Generator;
use crate::semantic_bridge::{BridgeError, BridgeResult};
use epsh::error::ExitStatus;
use epsh::shell_bytes::ShellBytes;
use epsh::var::Variables;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone)]
enum Expr {
    Lit(i64),
    Var(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Assign(String, Box<Expr>),
    Compound(&'static str, String, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

impl Expr {
    fn tree(&self) -> Value {
        match self {
            Self::Lit(value) => json!(["lit", value]),
            Self::Var(name) => json!(["var", name]),
            Self::Unary(operator, value) => json!(["unary", operator, value.tree()]),
            Self::Binary(operator, left, right) => {
                json!(["binary", operator, left.tree(), right.tree()])
            }
            Self::Assign(name, value) => json!(["assign", name, value.tree()]),
            Self::Compound(operator, name, value) => {
                json!(["compound", operator, name, value.tree()])
            }
            Self::And(left, right) => json!(["and", left.tree(), right.tree()]),
            Self::Or(left, right) => json!(["or", left.tree(), right.tree()]),
            Self::If(condition, yes, no) => {
                json!(["if", condition.tree(), yes.tree(), no.tree()])
            }
        }
    }

    fn source(&self) -> String {
        match self {
            Self::Lit(value) => value.to_string(),
            Self::Var(name) => name.clone(),
            Self::Unary(operator, value) => format!("{operator}({})", value.source()),
            Self::Binary(operator, left, right) => {
                format!("({} {operator} {})", left.source(), right.source())
            }
            Self::Assign(name, value) => format!("({name} = {})", value.source()),
            Self::Compound(operator, name, value) => {
                format!("({name} {operator}= {})", value.source())
            }
            Self::And(left, right) => format!("({} && {})", left.source(), right.source()),
            Self::Or(left, right) => format!("({} || {})", left.source(), right.source()),
            Self::If(condition, yes, no) => {
                format!("({} ? {} : {})", condition.source(), yes.source(), no.source())
            }
        }
    }

}

fn generated_expr(random: &mut Generator, depth: usize) -> Expr {
    let variables = ["v0", "v1", "v2"];
    if depth == 0 {
        return match random.below(3) {
            0 => Expr::Lit(random.below(100) as i64),
            _ => Expr::Var(variables[random.below(variables.len() as u64) as usize].into()),
        };
    }
    match random.below(12) {
        0 => Expr::Unary(
            ["+", "-", "!", "~"][random.below(4) as usize],
            Box::new(generated_expr(random, depth - 1)),
        ),
        1..=6 => {
            let operators = ["+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^", "==", "!=", "<", "<=", ">", ">="];
            let operator = operators[random.below(operators.len() as u64) as usize];
            let left = Box::new(generated_expr(random, depth - 1));
            let right = if matches!(operator, "/" | "%") {
                Box::new(Expr::Lit(random.below(5) as i64))
            } else if matches!(operator, "<<" | ">>") {
                Box::new(Expr::Lit(random.below(65) as i64))
            } else {
                Box::new(generated_expr(random, depth - 1))
            };
            Expr::Binary(operator, left, right)
        }
        7 => Expr::Assign(
            variables[random.below(variables.len() as u64) as usize].into(),
            Box::new(generated_expr(random, depth - 1)),
        ),
        8 => {
            let operators = ["+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^"];
            let operator = operators[random.below(operators.len() as u64) as usize];
            let name = variables[random.below(variables.len() as u64) as usize].into();
            let rhs = if matches!(operator, "/" | "%") {
                Box::new(Expr::Lit(random.below(5) as i64 + 1))
            } else if matches!(operator, "<<" | ">>") {
                Box::new(Expr::Lit(random.below(65) as i64))
            } else {
                Box::new(generated_expr(random, depth - 1))
            };
            Expr::Compound(operator, name, rhs)
        }
        9 => Expr::And(
            Box::new(generated_expr(random, depth - 1)),
            Box::new(generated_expr(random, depth - 1)),
        ),
        10 => Expr::Or(
            Box::new(generated_expr(random, depth - 1)),
            Box::new(generated_expr(random, depth - 1)),
        ),
        _ => Expr::If(
            Box::new(generated_expr(random, depth - 1)),
            Box::new(generated_expr(random, depth - 1)),
            Box::new(generated_expr(random, depth - 1)),
        ),
    }
}

pub(crate) fn generate(index: usize, seed: u64) -> Request {
    let mut random = Generator::new(seed ^ (index as u64).wrapping_mul(0x9e3779b97f4a7c15));
    let expr = if index < 8 {
        fixed_expr(index)
    } else {
        generated_expr(&mut random, 5)
    };
    let source = expr.source();
    let input = json!({"tree": expr.tree()});
    let variables = vec![
        ("v0", b"7".to_vec()),
        ("v1", if index.is_multiple_of(7) { vec![255] } else { b"-3".to_vec() }),
        ("v2", if index.is_multiple_of(11) { b"not-an-int".to_vec() } else { b"9223372036854775808".to_vec() }),
        ("min", i64::MIN.to_string().into_bytes()),
    ];
    let state = json!({
        "variables": variables,
        "exported": [["v0", [55]], ["v1", [45, 51]]],
        "inherited": [],
        "cwd": [47],
        "positional": [],
        "last_status": 0,
    });
    Request {
        id: format!("arithmetic-{index:05}"),
        family: Family::Arithmetic,
        source: source.into_bytes(),
        state,
        fuel: 128,
        input,
    }
}

fn fixed_expr(index: usize) -> Expr {
    match index {
        0 => Expr::Binary("+", Box::new(Expr::Lit(i64::MAX)), Box::new(Expr::Lit(1))),
        1 => Expr::Unary("-", Box::new(Expr::Var("min".into()))),
        2 => Expr::Binary(
            "/",
            Box::new(Expr::Var("min".into())),
            Box::new(Expr::Unary("-", Box::new(Expr::Lit(1)))),
        ),
        3 => Expr::Binary("%", Box::new(Expr::Lit(5)), Box::new(Expr::Lit(0))),
        4 => Expr::Binary("<<", Box::new(Expr::Lit(1)), Box::new(Expr::Lit(64))),
        5 => Expr::And(
            Box::new(Expr::Lit(0)),
            Box::new(Expr::Assign(
                "v0".into(),
                Box::new(Expr::Binary(
                    "/",
                    Box::new(Expr::Lit(1)),
                    Box::new(Expr::Lit(0)),
                )),
            )),
        ),
        6 => Expr::Compound("+", "v0".into(), Box::new(Expr::Lit(1))),
        _ => Expr::If(
            Box::new(Expr::Lit(1)),
            Box::new(Expr::Var("v1".into())),
            Box::new(Expr::Assign("v2".into(), Box::new(Expr::Lit(4)))),
        ),
    }
}

pub(crate) fn actual(request: &Request) -> BridgeResult<Value> {
    let source = std::str::from_utf8(&request.source)
        .map_err(|_| BridgeError::new("arithmetic source is not UTF-8"))?;
    let initial = request.state["variables"]
        .as_array()
        .ok_or_else(|| BridgeError::new("arithmetic state variables are malformed"))?;
    let mut variables = Variables::new_clean();
    let mut names = BTreeSet::new();
    for binding in initial {
        if binding.as_array().is_none_or(|pair| pair.len() != 2) {
            return Err(BridgeError::new("arithmetic binding is not a pair"));
        }
        let name = binding[0]
            .as_str()
            .ok_or_else(|| BridgeError::new("arithmetic variable name is not text"))?;
        let bytes = super::protocol::require_bytes(&binding[1], "arithmetic variable")?;
        variables
            .set_bytes(name, ShellBytes::from_vec(bytes))
            .map_err(BridgeError::new)?;
        names.insert(name.to_string());
    }
    let tree = request.input["tree"].clone();
    collect_writes(&tree, &mut names)?;
    let mut write_names = BTreeSet::new();
    collect_writes(&request.input["tree"], &mut write_names)?;
    let touched: Vec<String> = write_names.into_iter().collect();
    let result = epsh::arith::eval_arith(source, &mut variables, ExitStatus::SUCCESS, 1);
    let bindings = bindings_for(&variables, names.iter().cloned());
    match result {
        Ok(value) => Ok(json!({
            "tag": "success",
            "value": {"value": value, "variables": bindings},
        })),
        Err(message) => {
            let kind = arithmetic_error_code(&message, source);
            let state = update_arithmetic_state(&request.state, &variables, &touched)?;
            Ok(json!({
                "tag": "semantic_error",
                "error": {"kind": "arithmetic", "message": kind},
                "state": state,
                "trace": [],
            }))
        }
    }
}

fn collect_writes(tree: &Value, names: &mut BTreeSet<String>) -> BridgeResult<()> {
    let list = tree
        .as_array()
        .ok_or_else(|| BridgeError::new("arithmetic tree is malformed"))?;
    let tag = list.first().and_then(Value::as_str).unwrap_or("");
    match tag {
        "assign" | "compound" => {
            let name_index = if tag == "assign" { 1 } else { 2 };
            let name = list
                .get(name_index)
                .and_then(Value::as_str)
                .ok_or_else(|| BridgeError::new("arithmetic write has no name"))?;
            names.insert(name.to_string());
            let rhs = list
                .get(name_index + 1)
                .ok_or_else(|| BridgeError::new("arithmetic write has no rhs"))?;
            collect_writes(rhs, names)?;
        }
        "unary" => collect_writes(&list[2], names)?,
        "binary" => {
            collect_writes(&list[2], names)?;
            collect_writes(&list[3], names)?;
        }
        "and" | "or" => {
            collect_writes(&list[1], names)?;
            collect_writes(&list[2], names)?;
        }
        "if" => {
            collect_writes(&list[1], names)?;
            collect_writes(&list[2], names)?;
            collect_writes(&list[3], names)?;
        }
        "lit" | "var" => {}
        _ => return Err(BridgeError::new(format!("unknown arithmetic tree tag {tag:?}"))),
    }
    Ok(())
}

fn bindings_for(variables: &Variables, names: impl IntoIterator<Item = String>) -> Value {
    let mut bindings: Vec<(String, Vec<u8>)> = names
        .into_iter()
        .filter_map(|name| {
            variables
                .get_bytes(&name)
                .map(|value| (name, value.as_bytes().to_vec()))
        })
        .collect();
    bindings.sort_by(|left, right| left.0.cmp(&right.0));
    json!(bindings)
}

fn update_arithmetic_state(state: &Value, variables: &Variables, touched: &[String]) -> BridgeResult<Value> {
    let existing: Vec<String> = state["variables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| pair[0].as_str().map(ToOwned::to_owned))
        .collect();
    let names: BTreeSet<String> = existing.into_iter().chain(touched.iter().cloned()).collect();
    let mut updated = state.clone();
    updated["variables"] = bindings_for(variables, names.iter().cloned());
    let mut exported = Vec::new();
    for pair in state["exported"]
        .as_array()
        .ok_or_else(|| BridgeError::new("arithmetic exported state is malformed"))?
    {
        let name = pair[0]
            .as_str()
            .ok_or_else(|| BridgeError::new("exported variable name is not text"))?;
        let value = variables
            .get_bytes(name)
            .map(|bytes| bytes.as_bytes().to_vec())
            .map(Ok)
            .unwrap_or_else(|| super::protocol::require_bytes(&pair[1], "exported value"))?;
        exported.push((name.to_string(), value));
    }
    exported.sort_by(|left, right| left.0.cmp(&right.0));
    updated["exported"] = json!(exported);
    Ok(updated)
}

fn arithmetic_error_code(message: &str, source: &str) -> &'static str {
    if message.contains("division by zero") {
        "division_by_zero"
    } else if message.contains("division overflow") {
        "division_overflow"
    } else if message.contains("shift") {
        "invalid_shift"
    } else if source.contains("9223372036854775808") {
        "literal_range"
    } else {
        "syntax"
    }
}

pub(crate) fn negative_control(outcome: &Value) -> BridgeResult<Value> {
    let mut altered = outcome.clone();
    match altered["tag"].as_str() {
        Some("success") => {
            let value = altered["value"]["value"]
                .as_i64()
                .ok_or_else(|| BridgeError::new("arithmetic outcome has no value"))?;
            altered["value"]["value"] = json!(value.wrapping_add(1));
        }
        Some("semantic_error") => altered["error"]["kind"] = json!("environment"),
        _ => return Err(BridgeError::new("arithmetic negative control needs a result")),
    }
    Ok(altered)
}

pub(crate) fn shrink(request: &Request) -> BridgeResult<Vec<Request>> {
    let tree = request
        .input
        .get("tree")
        .ok_or_else(|| BridgeError::new("arithmetic input has no tree"))?;
    let mut trees = Vec::new();
    reduced_trees(tree, &mut trees);
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for tree in trees {
        if !seen.insert(tree.clone()) {
            continue;
        }
        let mut candidate = request.clone();
        candidate.input["tree"] = tree.clone();
        candidate.source = tree_source(&tree)?.into_bytes();
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn reduced_trees(tree: &Value, candidates: &mut Vec<Value>) {
    let Some(items) = tree.as_array() else {
        return;
    };
    let tag = items.first().and_then(Value::as_str).unwrap_or("");
    if !matches!(tag, "lit" | "var") {
        candidates.push(json!(["lit", 0]));
        candidates.push(json!(["lit", 1]));
    }
    let child_indices: &[usize] = match tag {
        "unary" => &[2],
        "binary" => &[2, 3],
        "assign" => &[2],
        "compound" => &[3],
        "and" | "or" => &[1, 2],
        "if" => &[1, 2, 3],
        _ => &[],
    };
    for index in child_indices {
        let Some(child) = items.get(*index) else {
            continue;
        };
        let mut child_reductions = Vec::new();
        reduced_trees(child, &mut child_reductions);
        for replacement in child_reductions {
            let mut reduced = items.clone();
            reduced[*index] = replacement;
            candidates.push(Value::Array(reduced));
        }
    }
}

fn tree_source(tree: &Value) -> BridgeResult<String> {
    let items = tree
        .as_array()
        .ok_or_else(|| BridgeError::new("arithmetic tree is not an array"))?;
    let tag = items
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| BridgeError::new("arithmetic tree has no tag"))?;
    let child = |index: usize| -> BridgeResult<String> {
        tree_source(
            items
                .get(index)
                .ok_or_else(|| BridgeError::new("arithmetic tree has a missing child"))?,
        )
    };
    Ok(match tag {
        "lit" => items[1]
            .as_i64()
            .ok_or_else(|| BridgeError::new("arithmetic literal is not an integer"))?
            .to_string(),
        "var" => items[1]
            .as_str()
            .ok_or_else(|| BridgeError::new("arithmetic variable is not text"))?
            .to_string(),
        "unary" => format!(
            "{}({})",
            items[1]
                .as_str()
                .ok_or_else(|| BridgeError::new("arithmetic unary operator is not text"))?,
            child(2)?
        ),
        "binary" => format!(
            "({} {} {})",
            child(2)?,
            items[1]
                .as_str()
                .ok_or_else(|| BridgeError::new("arithmetic operator is not text"))?,
            child(3)?
        ),
        "assign" => format!(
            "({} = {})",
            items[1]
                .as_str()
                .ok_or_else(|| BridgeError::new("arithmetic target is not text"))?,
            child(2)?
        ),
        "compound" => format!(
            "({} {}= {})",
            items[2]
                .as_str()
                .ok_or_else(|| BridgeError::new("arithmetic target is not text"))?,
            items[1]
                .as_str()
                .ok_or_else(|| BridgeError::new("arithmetic operator is not text"))?,
            child(3)?
        ),
        "and" => format!("({} && {})", child(1)?, child(2)?),
        "or" => format!("({} || {})", child(1)?, child(2)?),
        "if" => format!("({} ? {} : {})", child(1)?, child(2)?, child(3)?),
        _ => return Err(BridgeError::new(format!("unknown arithmetic tree tag {tag:?}"))),
    })
}

fn source_tree_depth(tree: &Value) -> usize {
    tree.as_array()
        .map(|items| 1 + items.iter().filter(|value| value.is_array()).map(source_tree_depth).max().unwrap_or(0))
        .unwrap_or(0)
}

pub(crate) fn validate_case(request: &Request) -> BridgeResult<()> {
    let tree = request.input.get("tree").ok_or_else(|| BridgeError::new("arithmetic input has no tree"))?;
    if source_tree_depth(tree) > 8 {
        return Err(BridgeError::new("generated arithmetic tree exceeds depth 8"));
    }
    if request.source.len() > 2048 {
        return Err(BridgeError::new("generated arithmetic source exceeds 2048 bytes"));
    }
    Ok(())
}

