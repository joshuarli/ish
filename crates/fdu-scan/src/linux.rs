use crate::fsutil::retry;
use rustix::fs::{self, Statx, StatxFlags};
use std::os::fd::BorrowedFd;

pub(crate) mod platform;

const EXT4_SUPER_MAGIC: u64 = 0xef53;
pub(crate) const EXT4_HTREE_EOF_32BIT: u64 = 0x7fff_ffff;
pub(crate) const EXT4_HTREE_EOF_64BIT: u64 = i64::MAX as u64;

fn reports(stat: &Statx, flags: StatxFlags) -> bool {
    StatxFlags::from_bits_retain(stat.stx_mask).contains(flags)
}

pub(super) fn is_ext4(fd: BorrowedFd<'_>) -> bool {
    retry(|| fs::fstatfs(fd)).is_ok_and(|filesystem| filesystem.f_type as u64 == EXT4_SUPER_MAGIC)
}
