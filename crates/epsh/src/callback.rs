use crate::error::{Result, ShellError};
use std::mem::ManuallyDrop;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackKind {
    External,
    CommandPolicy,
    RedirectOpen,
}

impl std::fmt::Display for CallbackKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::External => "external handler",
            Self::CommandPolicy => "command policy",
            Self::RedirectOpen => "redirect opener",
        })
    }
}

pub(crate) struct Registered<T> {
    owner_pid: u32,
    child_allowed: bool,
    callback: ManuallyDrop<T>,
}

impl<T> Registered<T> {
    pub(crate) fn new(callback: T) -> Self {
        Self { owner_pid: std::process::id(), child_allowed: false, callback: ManuallyDrop::new(callback) }
    }

    pub(crate) unsafe fn in_children(callback: T) -> Self {
        Self { owner_pid: std::process::id(), child_allowed: true, callback: ManuallyDrop::new(callback) }
    }

    pub(crate) fn check(&self, kind: CallbackKind) -> Result<()> {
        if self.owner_pid != std::process::id() && !self.child_allowed {
            Err(ShellError::CallbackProcess { kind })
        } else { Ok(()) }
    }

    pub(crate) fn get(&mut self, kind: CallbackKind) -> Result<&mut T> {
        self.check(kind)?;
        Ok(&mut self.callback)
    }
}

impl<T> Drop for Registered<T> {
    fn drop(&mut self) {
        if self.owner_pid == std::process::id() || self.child_allowed {
            // SAFETY: the payload is dropped once, only in its owner process or under the audited child-execution contract.
            unsafe { ManuallyDrop::drop(&mut self.callback); }
        }
    }
}
