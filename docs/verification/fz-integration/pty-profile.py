import os, pty, fcntl, termios, select, subprocess, time, pathlib, sys, json, resource
base, candidate, scratch = map(pathlib.Path, sys.argv[1:])
import tempfile
root = pathlib.Path(tempfile.mkdtemp(prefix='fz-pty-profile-', dir=scratch))
commands = [f'git checkout -b feature/task-{i}' if i % 2 else f'git commit -m fix-{i}' for i in range(45000)]

def run(binary, label, index):
    home = root / f'{label}-{index}'
    (home / '.local/share/ish').mkdir(parents=True, exist_ok=True)
    (home / '.config/ish').mkdir(parents=True, exist_ok=True)
    (home / '.local/share/ish/history').write_text('\n'.join(commands)+'\n')
    master, slave = pty.openpty()
    import struct
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
    def controlling(): fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    environment = os.environ.copy()
    environment.update(HOME=str(home), USER='profile', PATH='/usr/bin:/bin', ISH_TEST_SUPPRESS_OSC133='1', TMPDIR=str(scratch))
    started = time.monotonic_ns()
    child = subprocess.Popen([str(binary), '--no-config'], stdin=slave, stdout=slave, stderr=slave, cwd=home, env=environment, start_new_session=True, preexec_fn=controlling)
    os.close(slave)
    pending = b''
    def until(marker):
        nonlocal pending
        data = pending
        pending = b''
        deadline = time.monotonic()+15
        while marker not in data:
            remaining = deadline-time.monotonic()
            if remaining <= 0: raise RuntimeError(f'timeout: {label} {data[-1000:]!r}')
            ready, _, _ = select.select([master], [], [], remaining)
            if ready:
                part = os.read(master, 65536)
                if not part: raise RuntimeError('unexpected EOF')
                data += part
        end = data.index(marker) + len(marker)
        pending = data[end:]
        return data[:end]
    try:
        until(b'$ ')
        startup = time.monotonic_ns()-started
        start_search = time.monotonic_ns()
        os.write(master, b'\x12gco')
        until(b'search: \x1b[0mgco')
        search = time.monotonic_ns()-start_search
        os.write(master, b'\x1b')
        until(b'$ ')
        os.write(master, b"printf '%s\\n' RSS_''PROBE_OK\r")
        until(b'RSS_PROBE_OK')
        until(b'$ ')
        os.write(master, b'\x04')
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
        assert child.returncode == 0, child.returncode
        return dict(side=label, run=index, startup_ns=startup, search_ns=search, maxrss=usage.ru_maxrss, user_s=usage.ru_utime, system_s=usage.ru_stime)
    finally:
        if child.returncode is None:
            child.kill(); child.wait()
        os.close(master)

for index in range(5):
    for label, binary in [('baseline',base),('candidate',candidate)]:
        print(json.dumps(run(binary,label,index)), flush=True)
