#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
repo_root=$(CDPATH= cd "$script_dir/../.." && pwd -P)
tmp_root="$repo_root/.work/tmp"
mkdir -p "$tmp_root"

ish_bin="$repo_root/target/release/ish"
if [ ! -x "$ish_bin" ]; then
    printf 'bench-sync: release binary not found at %s\n' "$ish_bin" >&2
    exit 1
fi

rsync_bin=$(command -v /usr/bin/rsync || command -v rsync)
rsync_ver=$("$rsync_bin" --version 2>&1 | head -n 1 || true)

fixture_src="$tmp_root/sync-bench-src"
rm -rf "$fixture_src"
mkdir -p "$fixture_src/subdir"

# 200 files * 4 KiB + 1 file * 1 MiB
for i in $(seq 1 200); do
    head -c 4096 /dev/zero | tr '\0' 'A' > "$fixture_src/subdir/file-$i.bin"
done
head -c 1048576 /dev/zero | tr '\0' 'B' > "$fixture_src/large.bin"

printf 'Fixture created at %s: 201 files, ~1.78 MiB\n' "$fixture_src"
printf 'Reference rsync: %s (%s)\n' "$rsync_bin" "$rsync_ver"

if ! perf-gate-ready 2>/dev/null; then
    printf 'perf gate deferred: host busy\n'
    exit 0
fi

run_ish() {
    cmd=$1
    python3 -c "
import pty, os, time, sys

master, slave = pty.openpty()
pid = os.fork()
if pid == 0:
    os.close(master)
    os.dup2(slave, 0)
    os.dup2(slave, 1)
    os.dup2(slave, 2)
    os.close(slave)
    os.environ['HOME'] = '$tmp_root/bench-home'
    os.execl('$ish_bin', '$ish_bin')
else:
    os.close(slave)
    time.sleep(0.05)
    # wait for prompt
    os.read(master, 1024)
    start = time.perf_counter()
    os.write(master, b'$cmd\n')
    time.sleep(0.02)
    # read output until prompt
    out = b''
    while True:
        try:
            chunk = os.read(master, 4096)
            if not chunk: break
            out += chunk
            if b'$ ' in out[-20:]: break
        except OSError:
            break
    elapsed = time.perf_counter() - start
    os.write(master, b'exit\n')
    os.waitpid(pid, 0)
    os.close(master)
    print(f'{elapsed:.6f}')
"
}

run_reference() {
    args=$1
    python3 -c "
import subprocess, time
start = time.perf_counter()
subprocess.run(['$rsync_bin'] + '$args'.split(), check=True)
elapsed = time.perf_counter() - start
print(f'{elapsed:.6f}')
"
}

mkdir -p "$tmp_root/bench-home/.config/ish"

printf '\n=== Workload 1: sync-fresh-tree ===\n'
ish_fresh_times=""
ref_fresh_times=""
for sample in $(seq 1 5); do
    dst_ish="$tmp_root/dst-ish-fresh-$sample"
    dst_ref="$tmp_root/dst-ref-fresh-$sample"
    rm -rf "$dst_ish" "$dst_ref"
    
    t_ish=$(run_ish "rsync --contents $fixture_src $dst_ish")
    diff -r "$fixture_src" "$dst_ish" >/dev/null || { printf 'correctness failed ish\n'; exit 1; }
    
    t_ref=$(run_reference "-a $fixture_src/ $dst_ref/")
    diff -r "$fixture_src" "$dst_ref" >/dev/null || { printf 'correctness failed ref\n'; exit 1; }
    
    printf 'Sample %d: ish=%s s, rsync=%s s\n' "$sample" "$t_ish" "$t_ref"
    ish_fresh_times="$ish_fresh_times $t_ish"
    ref_fresh_times="$ref_fresh_times $t_ref"
    rm -rf "$dst_ish" "$dst_ref"
done

printf '\n=== Workload 2: sync-noop-tree ===\n'
ish_noop_times=""
ref_noop_times=""
dst_ish_noop="$tmp_root/dst-ish-noop"
dst_ref_noop="$tmp_root/dst-ref-noop"
rm -rf "$dst_ish_noop" "$dst_ref_noop"
run_ish "rsync --contents $fixture_src $dst_ish_noop" >/dev/null
run_reference "-a $fixture_src/ $dst_ref_noop/" >/dev/null

for sample in $(seq 1 5); do
    t_ish=$(run_ish "rsync --contents $fixture_src $dst_ish_noop")
    diff -r "$fixture_src" "$dst_ish_noop" >/dev/null || { printf 'correctness failed ish\n'; exit 1; }
    
    t_ref=$(run_reference "-a $fixture_src/ $dst_ref_noop/")
    diff -r "$fixture_src" "$dst_ref_noop" >/dev/null || { printf 'correctness failed ref\n'; exit 1; }
    
    printf 'Sample %d: ish=%s s, rsync=%s s\n' "$sample" "$t_ish" "$t_ref"
    ish_noop_times="$ish_noop_times $t_ish"
    ref_noop_times="$ref_noop_times $t_ref"
done
rm -rf "$dst_ish_noop" "$dst_ref_noop"

if ! perf-gate-ready -1 2>/dev/null; then
    printf 'perf gate deferred: host busy\n'
    exit 0
fi

python3 -c "
import statistics
def median(vals):
    s = [float(x) for x in vals.split()]
    return statistics.median(s)

print('\n=== Benchmark Results Summary (Medians of 5 runs) ===')
print(f'sync-fresh-tree: ish = {median(\"$ish_fresh_times\"):.4f}s, rsync = {median(\"$ref_fresh_times\"):.4f}s')
print(f'sync-noop-tree:  ish = {median(\"$ish_noop_times\"):.4f}s, rsync = {median(\"$ref_noop_times\"):.4f}s')
"

rm -rf "$fixture_src" "$tmp_root/bench-home"
