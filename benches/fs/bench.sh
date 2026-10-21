#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
repo_root=$(CDPATH= cd "$script_dir/../.." && pwd -P)
tmp_root="$repo_root/.work/tmp"
target_root="$repo_root/.work/bench-target"
export TMPDIR="$tmp_root"
export CARGO_TARGET_DIR="$target_root"
if ! git -C "$repo_root" check-ignore -q .work/tmp; then
    printf '%s\n' 'bench.sh: .work/tmp is not ignored by .git/info/exclude' >&2
    exit 2
fi
mkdir -p "$tmp_root"

run_helper() {
    cargo run --quiet -p ish-fs --example fs_bench -- "$@"
}

usage() {
    printf '%s\n' \
        'usage:' \
        '  bench.sh prepare ROOT [CROSS_FILESYSTEM_PARENT]' \
        '  bench.sh verify ROOT DEVICE:INODE [CROSS_ROOT CROSS_DEVICE:INODE]' \
        '  bench.sh cleanup ROOT DEVICE:INODE [CROSS_ROOT CROSS_DEVICE:INODE]' \
        '  bench.sh host ROOT' \
        '  bench.sh measure ROOT DEVICE:INODE WORKLOAD [CROSS_ROOT CROSS_DEVICE:INODE]'
}

command_name=${1-}
if [ "$#" -gt 0 ]; then
    shift
fi

case "$command_name" in
    prepare)
        [ "$#" -ge 1 ] && [ "$#" -le 2 ] || { usage >&2; exit 2; }
        run_helper prepare "$@"
        ;;
    verify)
        [ "$#" -ge 2 ] && [ "$#" -le 4 ] || { usage >&2; exit 2; }
        run_helper verify "$@"
        ;;
    cleanup)
        [ "$#" -ge 2 ] && [ "$#" -le 4 ] || { usage >&2; exit 2; }
        run_helper cleanup "$@"
        ;;
    host)
        [ "$#" -eq 1 ] || { usage >&2; exit 2; }
        fixture_root=$1
        printf 'platform=%s\n' "$(uname -s)"
        printf 'architecture=%s\n' "$(uname -m)"
        printf 'filesystem_device=%s\n' "$(stat -c %d "$fixture_root" 2>/dev/null || stat -f %d "$fixture_root")"
        case "$(uname -s)" in
            Linux)
                if command -v findmnt >/dev/null 2>&1; then
                    printf 'filesystem_type=%s\n' "$(findmnt -no FSTYPE -T "$fixture_root")"
                else
                    printf 'filesystem_type=%s\n' "$(stat -f -c %T "$fixture_root")"
                fi
                ;;
            Darwin) printf 'filesystem_type=%s\n' "$(stat -f %T "$fixture_root")" ;;
            *) printf '%s\n' 'filesystem_type=unknown' ;;
        esac
        printf 'filesystem_mount=%s\n' "$(df -P "$fixture_root" | tail -n 1)"
        cp_path=$(command -v cp || true)
        cp_text=$(cp --version 2>&1 | head -n 2 || true)
        cp_help=$(cp --help 2>&1 || true)
        case "$cp_text" in
            *'GNU coreutils'*)
                case "$cp_help" in
                    *--reflink*) printf 'native_cp=gnu-coreutils path=%s clone_flag=--reflink=auto\n' "$cp_path" ;;
                    *) printf 'native_cp=gnu-coreutils path=%s clone_flag=unavailable\n' "$cp_path" ;;
                esac
                ;;
            *'BusyBox'*) printf 'native_cp=unavailable-clone-aware path=%s detail=BusyBox\n' "$cp_path" ;;
            *)
                case "$(uname -s)" in
                    Darwin) printf 'native_cp=apple-cp path=%s clone_flag=-c\n' "$cp_path" ;;
                    *) printf 'native_cp=unclassified path=%s detail=%s\n' "$cp_path" "$(printf '%s' "$cp_text" | tr '\n' ' ')" ;;
                esac
                ;;
        esac
        mv_path=$(command -v mv || true)
        mv_text=$(mv --version 2>&1 | head -n 2 || true)
        case "$mv_text" in
            *'GNU coreutils'*) printf 'native_mv=gnu-coreutils path=%s durability=weaker-than-ish-fs\n' "$mv_path" ;;
            *'BusyBox'*) printf 'native_mv=busybox path=%s durability=weaker-than-ish-fs\n' "$mv_path" ;;
            *)
                case "$(uname -s)" in
                    Darwin) printf 'native_mv=macos path=%s durability=weaker-than-ish-fs\n' "$mv_path" ;;
                    *) printf 'native_mv=unclassified path=%s detail=%s\n' "$mv_path" "$(printf '%s' "$mv_text" | tr '\n' ' ')" ;;
                esac
                ;;
        esac
        printf '%s\n' 'clone_filesystem=unprobed use_observed_engine_method_counts'
        printf '%s\n' 'cross_filesystem=unavailable unless prepare receives a distinct-device parent below .work/tmp'
        ;;
    measure)
        exec "$script_dir/measure.sh" "$@"
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
