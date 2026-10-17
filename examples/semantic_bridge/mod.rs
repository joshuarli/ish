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
pub(crate) struct BridgeError(String);

impl BridgeError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
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
