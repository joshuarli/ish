use super::{Backend, Metadata, Pid};
use std::collections::HashSet;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(super) struct Native;

fn bounded(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 { return Err(io::Error::other("process metadata exceeds 8 MiB")); }
    Ok(bytes)
}

fn fields(stat: &[u8]) -> io::Result<(i32, u64)> {
    let end = stat.iter().rposition(|&byte| byte == b')').ok_or_else(|| io::Error::other("malformed proc stat name"))?;
    let fields: Vec<_> = stat[end + 1..].split(u8::is_ascii_whitespace).filter(|field| !field.is_empty()).collect();
    let decimal = |index: usize| fields.get(index).and_then(|bytes| std::str::from_utf8(bytes).ok()).and_then(|text| text.parse::<u64>().ok()).ok_or_else(|| io::Error::other("malformed proc stat identity"));
    let parent = i32::try_from(decimal(1)?).map_err(io::Error::other)?;
    Ok((parent, decimal(19)?))
}

fn credentials(status: &[u8]) -> io::Result<(u32, u32)> {
    let line = status.split(|&byte| byte == b'\n').find_map(|line| line.strip_prefix(b"Uid:")).ok_or_else(|| io::Error::other("proc status has no Uid"))?;
    let mut fields = line.split(u8::is_ascii_whitespace).filter(|field| !field.is_empty());
    let mut next = || fields.next().and_then(|bytes| std::str::from_utf8(bytes).ok()).and_then(|text| text.parse().ok()).ok_or_else(|| io::Error::other("malformed proc Uid"));
    Ok((next()?, next()?))
}

fn listener_inodes(table: &[u8], port: u16, output: &mut HashSet<u64>) -> io::Result<()> {
    for line in table.split(|&byte| byte == b'\n').skip(1).filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split(u8::is_ascii_whitespace).filter(|field| !field.is_empty()).collect();
        if fields.len() < 10 { return Err(io::Error::other("short proc TCP row")); }
        let local = fields[1].rsplit(|&byte| byte == b':').next().unwrap_or_default();
        let local = std::str::from_utf8(local).ok().and_then(|text| u16::from_str_radix(text, 16).ok()).ok_or_else(|| io::Error::other("malformed proc TCP port"))?;
        if local != port || fields[3] != b"0A" { continue; }
        let inode = std::str::from_utf8(fields[9]).ok().and_then(|text| text.parse::<u64>().ok()).filter(|&inode| inode != 0).ok_or_else(|| io::Error::other("malformed proc TCP inode"))?;
        output.insert(inode);
    }
    Ok(())
}

fn listeners(port: u16) -> io::Result<HashSet<u64>> {
    let mut output = HashSet::new();
    listener_inodes(&bounded("/proc/self/net/tcp")?, port, &mut output)?;
    match bounded("/proc/self/net/tcp6") {
        Ok(table) => listener_inodes(&table, port, &mut output)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(output)
}

impl Backend for Native {
    type Handle = OwnedFd;
    fn list(&self) -> io::Result<Vec<Pid>> {
        std::fs::read_dir("/proc")?.filter_map(|entry| match entry {
            Err(error) => Some(Err(error)),
            Ok(entry) => entry.file_name().to_str().and_then(|name| name.parse().ok()).and_then(|pid| Pid::new(pid).ok()).map(Ok),
        }).collect()
    }
    fn ancestry(&self, pid: Pid) -> io::Result<i32> {
        fields(&bounded(format!("/proc/{}/stat", pid.raw()))?).map(|(parent, _)| parent)
    }
    fn metadata(&self, pid: Pid) -> io::Result<Metadata> {
        let path = format!("/proc/{}", pid.raw());
        let first = fields(&bounded(format!("{path}/stat"))?)?;
        let (real_uid, effective_uid) = credentials(&bounded(format!("{path}/status"))?)?;
        let (executable, argv) = if effective_uid == rustix::process::geteuid().as_raw() {
            (std::fs::read_link(format!("{path}/exe"))?.as_os_str().as_bytes().to_vec(), bounded(format!("{path}/cmdline"))?)
        } else { (Vec::new(), Vec::new()) };
        if first != fields(&bounded(format!("{path}/stat"))?)? { return Err(io::Error::other("process changed during inspection")); }
        Ok(Metadata { pid, parent: first.0, real_uid, effective_uid, started: (first.1, 0), executable, argv })
    }
    fn bind(&self, record: &Metadata) -> io::Result<OwnedFd> {
        let fd = rustix::process::pidfd_open(record.pid.native(), rustix::process::PidfdFlags::empty()).map_err(|error| {
            if error == rustix::io::Errno::NOSYS { io::Error::new(io::ErrorKind::Unsupported, "pk requires Linux pidfd support (kernel 5.3 or newer)") }
            else { error.into() }
        })?;
        if self.metadata(record.pid)? != *record { return Err(io::Error::other("process changed while binding pidfd")); }
        Ok(fd)
    }
    fn listens(&self, pid: Pid, port: u16) -> io::Result<bool> {
        let inodes = listeners(port)?;
        if inodes.is_empty() { return Ok(false); }
        for entry in std::fs::read_dir(format!("/proc/{}/fd", pid.raw()))? {
            let path = entry?.path();
            let link = match std::fs::read_link(&path) {
                Ok(link) => link,
                Err(error) if super::gone(&error) => continue,
                Err(error) => return Err(error),
            };
            let bytes = link.as_os_str().as_bytes();
            let inode = bytes.strip_prefix(b"socket:[").and_then(|bytes| bytes.strip_suffix(b"]"))
                .and_then(|bytes| std::str::from_utf8(bytes).ok()).and_then(|text| text.parse::<u64>().ok());
            if let Some(inode) = inode && inodes.contains(&inode) && listeners(port)?.contains(&inode) {
                match std::fs::read_link(&path) {
                    Ok(current) if current == link => return Ok(true),
                    Ok(_) => {}
                    Err(error) if super::gone(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(false)
    }
    fn signal(&self, handle: &OwnedFd, signal: rustix::process::Signal) -> io::Result<()> {
        rustix::process::pidfd_send_signal(handle, signal).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_names_can_contain_spaces_and_parentheses() {
        let mut stat = b"123 (worker ) name (x)) S 42".to_vec();
        for _ in 0..17 { stat.extend_from_slice(b" 0"); }
        stat.extend_from_slice(b" 12345 0");
        assert_eq!(fields(&stat).unwrap(), (42, 12345));
        assert!(fields(b"123 broken").is_err());
    }
    #[test]
    fn tcp_table_requires_listen_and_local_port() {
        let mut result = HashSet::new();
        listener_inodes(b"header\n 0: 00000000:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 123\n 1: 00000000:1F90 0:0 01 0:0 0:0 0 1000 0 456\n", 8080, &mut result).unwrap();
        assert_eq!(result, HashSet::from([123]));
    }
}
