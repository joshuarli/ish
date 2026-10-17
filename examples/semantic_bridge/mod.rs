mod arithmetic;
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
    runner::run(args)
}
