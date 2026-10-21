use std::io;

pub(super) const ALLOCATION_OVERHEAD: usize = 64;

pub(super) struct CaptureBudget {
    pub(super) used: usize,
    pub(super) limit: usize,
}

impl CaptureBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    fn allocation_charge(capacity: usize) -> io::Result<usize> {
        if capacity == 0 {
            return Ok(0);
        }
        capacity.checked_add(ALLOCATION_OVERHEAD).ok_or_else(too_large)
    }

    pub(super) fn check(&self, capacity: usize) -> io::Result<()> {
        let charge = Self::allocation_charge(capacity)?;
        if self.used.checked_add(charge).is_none_or(|used| used > self.limit) {
            return Err(too_large());
        }
        Ok(())
    }

    pub(super) fn record(&mut self, capacity: usize) -> io::Result<()> {
        self.check(capacity)?;
        self.used += Self::allocation_charge(capacity)?;
        Ok(())
    }
}

pub(super) fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "extended metadata exceeds the platform capture limit",
    )
}

pub(super) fn allocation_error(error: std::collections::TryReserveError) -> io::Error {
    io::Error::new(io::ErrorKind::OutOfMemory, error)
}
