#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
repo_root=$(CDPATH= cd "$script_dir/../.." && pwd -P)
tmp_root="$repo_root/.work/tmp"
mkdir -p "$tmp_root"

fixture="$tmp_root/smoke-sync-src"
dst="$tmp_root/smoke-sync-dst"
rm -rf "$fixture" "$dst"
mkdir -p "$fixture/subdir"

for i in $(seq 1 50); do
    printf 'content-%03d\n' "$i" > "$fixture/file-$i.txt"
    printf 'sub-%03d\n' "$i" > "$fixture/subdir/sub-$i.txt"
done

reference_rsync=$(command -v /usr/bin/rsync || command -v rsync || true)
if [ -z "$reference_rsync" ]; then
    printf 'smoke-sync: no rsync binary found on host\n' >&2
    exit 1
fi

start_ts=$(date +%s)
"$reference_rsync" -a "$fixture/" "$dst/"
fresh_elapsed=$(($(date +%s) - start_ts))
diff -r "$fixture" "$dst" || {
    printf 'smoke-sync: fresh sync correctness failure\n' >&2
    exit 1
}
printf 'smoke-sync: fresh sync ok elapsed=%ds\n' "$fresh_elapsed"

start_ts=$(date +%s)
"$reference_rsync" -a "$fixture/" "$dst/"
noop_elapsed=$(($(date +%s) - start_ts))
diff -r "$fixture" "$dst" || {
    printf 'smoke-sync: noop sync correctness failure\n' >&2
    exit 1
}
printf 'smoke-sync: noop sync ok elapsed=%ds\n' "$noop_elapsed"

rm "$fixture/file-1.txt"
start_ts=$(date +%s)
"$reference_rsync" -a --delete "$fixture/" "$dst/"
delete_elapsed=$(($(date +%s) - start_ts))
diff -r "$fixture" "$dst" || {
    printf 'smoke-sync: delete sync correctness failure\n' >&2
    exit 1
}
if [ -e "$dst/file-1.txt" ]; then
    printf 'smoke-sync: delete sync failed to remove deleted file\n' >&2
    exit 1
fi
printf 'smoke-sync: delete sync ok elapsed=%ds\n' "$delete_elapsed"

rm -rf "$fixture" "$dst"
printf 'smoke-sync: all smoke tests passed\n'
