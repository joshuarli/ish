use super::{Backend, Metadata, Pid};
use std::io;
use std::mem::{size_of, MaybeUninit};

pub(super) struct Native;

fn bsd(pid: Pid) -> io::Result<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: libproc receives aligned writable storage of exactly the declared SDK type.
    let size = unsafe { libc::proc_pidinfo(pid.raw(), libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size_of::<libc::proc_bsdinfo>() as i32) };
    if size != size_of::<libc::proc_bsdinfo>() as i32 { return Err(io::Error::last_os_error()); }
    // SAFETY: the exact-size return initialized the complete record.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid.raw() as u32 { return Err(io::Error::other("libproc returned a different PID")); }
    Ok(info)
}

fn argv(pid: Pid) -> io::Result<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid.raw()];
    let mut data = vec![0u8; 1024 * 1024];
    let mut length = data.len();
    // SAFETY: the three-element MIB and output buffer are valid for sysctl. No new value is supplied.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, data.as_mut_ptr().cast(), &mut length, std::ptr::null_mut(), 0) } == -1 { return Err(io::Error::last_os_error()); }
    if length > data.len() || length < 4 { return Err(io::Error::other("invalid process argument size")); }
    data.truncate(length);
    parse_argv(&data)
}

fn parse_argv(data: &[u8]) -> io::Result<Vec<u8>> {
    let count = i32::from_ne_bytes(data.get(..4).ok_or_else(|| io::Error::other("short process arguments"))?.try_into().unwrap());
    if !(0..=262144).contains(&count) { return Err(io::Error::other("invalid process argument count")); }
    let mut rest = &data[4..];
    let executable_end = rest.iter().position(|&byte| byte == 0).ok_or_else(|| io::Error::other("unterminated process executable"))?;
    rest = &rest[executable_end + 1..];
    while rest.first() == Some(&0) { rest = &rest[1..]; }
    let mut output = Vec::new();
    for _ in 0..count {
        let end = rest.iter().position(|&byte| byte == 0).ok_or_else(|| io::Error::other("short process argument record"))?;
        output.extend_from_slice(&rest[..=end]);
        rest = &rest[end + 1..];
    }
    Ok(output)
}

// libproc socket layouts from sys/proc_info.h on the supported 64-bit macOS ABIs.
// Opaque fields preserve their SDK size and alignment; only TCP fields are read.
#[repr(C)]
#[derive(Clone, Copy)]
struct TcpInfo {
    foreign_port: i32,
    local_port: i32,
    generation: u64,
    flags: [u32; 2],
    version: u8,
    addresses: [u8; 55],
    state: i32,
    timers: [i32; 7],
    control_block: u64,
}
#[repr(C)]
union Protocol { tcp: TcpInfo, opaque: [u64; 66] }
#[repr(C)]
struct SocketInfo {
    file: [u64; 3],
    stat: [u64; 17],
    socket: u64,
    pcb: u64,
    kind_type: i32,
    protocol: i32,
    family: i32,
    state: [i16; 8],
    oobmark: u32,
    buffers: [u32; 12],
    kind: i32,
    reserved: u32,
    details: Protocol,
}
const _: () = assert!(size_of::<SocketInfo>() == 792);
const _: () = assert!(size_of::<TcpInfo>() == 120);

fn socket(pid: Pid, fd: i32) -> io::Result<SocketInfo> {
    let mut info = MaybeUninit::<SocketInfo>::zeroed();
    // SAFETY: SocketInfo matches socket_fdinfo's 64-bit SDK layout. The buffer is aligned and sized accordingly.
    let result = unsafe { libc::proc_pidfdinfo(pid.raw(), fd, 3, info.as_mut_ptr().cast(), size_of::<SocketInfo>() as i32) };
    if result != size_of::<SocketInfo>() as i32 { return Err(io::Error::last_os_error()); }
    // SAFETY: an exact-size return initializes the entire record; every field admits all bit patterns.
    Ok(unsafe { info.assume_init() })
}
fn listener(info: &SocketInfo, port: u16) -> Option<(u64, u64)> {
    if info.kind != 2 || info.kind_type != libc::SOCK_STREAM || info.protocol != libc::IPPROTO_TCP
        || !matches!(info.family, libc::AF_INET | libc::AF_INET6) { return None; }
    // SAFETY: SOCKINFO_TCP selects the tcp member of the ABI union.
    let tcp = unsafe { info.details.tcp };
    if tcp.state != 1 || tcp.version & 3 == 0 || u16::from_be(tcp.local_port as u16) != port { return None; }
    Some((info.socket, tcp.generation))
}

impl Backend for Native {
    type Handle = Pid;
    fn list(&self) -> io::Result<Vec<Pid>> {
        for _ in 0..4 {
            // SAFETY: a null output asks libproc for its current process count.
            let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
            if count <= 0 { return Err(io::Error::last_os_error()); }
            let capacity = usize::try_from(count).unwrap().checked_add(1024).filter(|&n| n <= 1048576).ok_or_else(|| io::Error::other("process list exceeds limit"))?;
            let mut pids = vec![0i32; capacity];
            // SAFETY: pids is writable for the supplied byte count.
            let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (capacity * size_of::<i32>()) as i32) };
            if count < 0 { return Err(io::Error::last_os_error()); }
            if count as usize >= capacity { continue; }
            pids.truncate(count as usize);
            return Ok(pids.into_iter().filter_map(|pid| Pid::new(pid).ok()).collect());
        }
        Err(io::Error::other("process list changed repeatedly; retry pk"))
    }
    fn ancestry(&self, pid: Pid) -> io::Result<i32> { bsd(pid).map(|info| info.pbi_ppid as i32) }
    fn metadata(&self, pid: Pid) -> io::Result<Metadata> {
        let first = bsd(pid)?;
        let (executable, argv) = if first.pbi_uid == rustix::process::geteuid().as_raw() {
            let mut path = vec![0u8; 4096];
            // SAFETY: path is writable for its declared length.
            let size = unsafe { libc::proc_pidpath(pid.raw(), path.as_mut_ptr().cast(), path.len() as u32) };
            if size <= 0 { return Err(io::Error::last_os_error()); }
            if size as usize >= path.len() { return Err(io::Error::other("truncated process executable")); }
            path.truncate(size as usize);
            if path.last() == Some(&0) { path.pop(); }
            (path, argv(pid)?)
        } else { (Vec::new(), Vec::new()) };
        let last = bsd(pid)?;
        if first.pbi_start_tvsec != last.pbi_start_tvsec || first.pbi_start_tvusec != last.pbi_start_tvusec
            || first.pbi_uid != last.pbi_uid || first.pbi_ruid != last.pbi_ruid || first.pbi_ppid != last.pbi_ppid
        { return Err(io::Error::other("process changed during inspection")); }
        Ok(Metadata { pid, parent: first.pbi_ppid as i32, real_uid: first.pbi_ruid, effective_uid: first.pbi_uid,
            started: (first.pbi_start_tvsec, first.pbi_start_tvusec), executable, argv })
    }
    fn bind(&self, record: &Metadata) -> io::Result<Pid> {
        if self.metadata(record.pid)? != *record { return Err(io::Error::other("process changed during snapshot")); }
        Ok(record.pid)
    }
    fn listens(&self, pid: Pid, port: u16) -> io::Result<bool> {
        for _ in 0..4 {
            // SAFETY: null output asks libproc for the required FD buffer size.
            let size = unsafe { libc::proc_pidinfo(pid.raw(), libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
            if size <= 0 {
                let error = io::Error::last_os_error();
                if size == 0 && bsd(pid)?.pbi_nfiles == 0 { return Ok(false); }
                return Err(error);
            }
            let capacity = (size as usize / size_of::<libc::proc_fdinfo>()).checked_add(64).filter(|&n| n <= 1048576).ok_or_else(|| io::Error::other("process FD list exceeds limit"))?;
            let mut fds = Vec::<libc::proc_fdinfo>::with_capacity(capacity);
            let bytes = capacity * size_of::<libc::proc_fdinfo>();
            // SAFETY: the allocation is aligned and writable for bytes; its length stays zero until validated.
            let result = unsafe { libc::proc_pidinfo(pid.raw(), libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), bytes as i32) };
            if result <= 0 {
                let error = io::Error::last_os_error();
                if result == 0 && bsd(pid)?.pbi_nfiles == 0 { return Ok(false); }
                return Err(error);
            }
            if result as usize >= bytes { continue; }
            if !(result as usize).is_multiple_of(size_of::<libc::proc_fdinfo>()) { return Err(io::Error::other("partial process FD record")); }
            // SAFETY: libproc initialized this many complete records within capacity.
            unsafe { fds.set_len(result as usize / size_of::<libc::proc_fdinfo>()); }
            for fd in fds {
                if fd.proc_fdtype != libc::PROX_FDTYPE_SOCKET as u32 { continue; }
                let first = match socket(pid, fd.proc_fd) {
                    Ok(info) => info,
                    Err(error) if super::gone(&error) || matches!(error.raw_os_error(), Some(libc::EBADF | libc::EINVAL)) => continue,
                    Err(error) => return Err(error),
                };
                if let Some(identity) = listener(&first, port) {
                    match socket(pid, fd.proc_fd) {
                        Ok(last) if listener(&last, port) == Some(identity) => return Ok(true),
                        Ok(_) => {}
                        Err(error) if super::gone(&error) || matches!(error.raw_os_error(), Some(libc::EBADF | libc::EINVAL)) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            return Ok(false);
        }
        Err(io::Error::other("process FD list changed repeatedly; retry pk"))
    }
    fn signal(&self, handle: &Pid, signal: rustix::process::Signal) -> io::Result<()> {
        rustix::process::kill_process(handle.native(), signal).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_process_socket_lookup_is_an_error() {
        assert!(Native.listens(Pid::new(i32::MAX).unwrap(), 8080).is_err());
    }
}
