mod arithmetic;
mod evaluation;
mod effects;
mod expansion;
mod interactive;
mod policy;
mod protocol;
mod random;
mod runner;

use std::error::Error;
use std::fmt;

pub(crate) type BridgeResult<T> = Result<T, BridgeError>;

#[derive(Debug)]
pub(crate) struct BridgeError {
    message: String,
    raw_filenames_unsupported: bool,
}

impl BridgeError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), raw_filenames_unsupported: false }
    }

    pub(crate) fn raw_filenames_unsupported(message: impl Into<String>) -> Self {
        Self { message: message.into(), raw_filenames_unsupported: true }
    }

    pub(crate) fn is_raw_filenames_unsupported(&self) -> bool {
        self.raw_filenames_unsupported
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BridgeError {}

pub(crate) fn run_from_args(args: impl IntoIterator<Item = String>) -> BridgeResult<()> {
    let args: Vec<String> = args.into_iter().collect();
    if args.first().is_some_and(|argument| argument == "--effects-worker") {
        if args.len() != 1 {
            return Err(BridgeError::new("effects worker does not accept arguments"));
        }
        return effects::subprocess_worker();
    }
    runner::run(args)
}

pub(crate) fn pairs<A, B>(items: impl IntoIterator<Item = (A, B)>) -> deser_value::Value
where
    A: Into<deser_value::Value>,
    B: Into<deser_value::Value>,
{
    items
        .into_iter()
        .map(|(first, second)| deser_value::Value::from(vec![first.into(), second.into()]))
        .collect()
}

pub(crate) fn render(value: &deser_value::Value) -> String {
    deser_json::to_string(value).expect("values are plain data")
}
