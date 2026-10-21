#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
repo_root=$(CDPATH= cd "$script_dir/../.." && pwd -P)
tmp_root="$repo_root/.work/tmp"
export TMPDIR="$tmp_root"
export CARGO_TARGET_DIR="$repo_root/.work/native-boundaries-target"

if [ "$(uname -s)" != Linux ]; then
    printf '%s\n' 'native-fixtures.sh: Linux is required for mount namespaces and statx mount IDs' >&2
    exit 2
fi

git -C "$repo_root" check-ignore -q .work/tmp || {
    printf '%s\n' 'native-fixtures.sh: .work/tmp is not ignored by .git/info/exclude' >&2
    exit 2
}
mkdir -p "$tmp_root"

if [ "${1-}" != --inside-private-namespace ]; then
    [ "$#" -eq 0 ] || {
        printf '%s\n' 'native-fixtures.sh: unexpected arguments' >&2
        exit 2
    }
    exec unshare -Ur -m --propagation private "$0" --inside-private-namespace
fi
[ "$#" -eq 1 ] || {
    printf '%s\n' 'native-fixtures.sh: invalid namespace invocation' >&2
    exit 2
}

fixture_root="$tmp_root/native-boundaries-$$"
cross_root="$fixture_root/cross"
source_root="$fixture_root/source"
tmpfs_mount="$source_root/tmpfs-tree/tmpfs-mounted"
bind_source="$fixture_root/bind-source"
bind_mount="$source_root/bind-tree/bind-mounted"
cross_mounted=0
tmpfs_mounted=0
bind_mounted=0

mkdir "$fixture_root"

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    cleanup_status=0
    if [ "$bind_mounted" -eq 1 ]; then
        umount "$bind_mount" || cleanup_status=1
    fi
    if [ "$tmpfs_mounted" -eq 1 ]; then
        umount "$tmpfs_mount" || cleanup_status=1
    fi
    if [ "$cross_mounted" -eq 1 ]; then
        umount "$cross_root" || cleanup_status=1
    fi
    if [ "$cleanup_status" -eq 0 ]; then
        rm -rf "$fixture_root"
    else
        printf 'native-fixtures.sh: fixture mounts remain under %s\n' "$fixture_root" >&2
        status=1
    fi
    if [ "$status" -ne 0 ]; then
        exit "$status"
    fi
    if [ "$cleanup_status" -ne 0 ]; then
        exit "$cleanup_status"
    fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$source_root/tmpfs-tree/tmpfs-mounted" "$source_root/bind-tree/bind-mounted" "$bind_source" "$cross_root"
mount -t tmpfs -o size=128m,nosuid,nodev,mode=0777 tmpfs "$cross_root"
cross_mounted=1
mount -t tmpfs -o size=32m,nosuid,nodev,mode=0777 tmpfs "$tmpfs_mount"
tmpfs_mounted=1
mount --bind "$bind_source" "$bind_mount"
bind_mounted=1

failures=0
if ISH_FS_TEST_CROSS_ROOT="$cross_root" \
    cargo test --locked -p ish-fs --test native_boundaries native_boundary_real_exdev_move_preserves_and_syncs -- --ignored --exact --nocapture; then
    :
else
    failures=1
fi
if ISH_FS_TEST_CROSS_ROOT="$cross_root" \
ISH_FS_TEST_NESTED_SOURCE="$source_root" \
ISH_FS_TEST_NESTED_MOUNT="$tmpfs_mount" \
ISH_FS_TEST_NESTED_BIND="$bind_mount" \
    cargo test --locked -p ish-fs --test native_boundaries native_boundary_nested_mount_requires_opt_in -- --ignored --exact --nocapture; then
    :
else
    failures=1
fi
if ISH_FS_TEST_ALIAS_ORIGIN="$bind_source" \
ISH_FS_TEST_ALIAS_BIND="$bind_mount" \
    cargo test --locked -p ish-fs --test native_boundaries native_boundary_bind_exdev_then_same_mount_alias_preserves_topology -- --ignored --exact --nocapture; then
    :
else
    failures=1
fi
exit "$failures"
