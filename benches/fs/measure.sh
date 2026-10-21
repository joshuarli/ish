#!/bin/sh
set -eu

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
repo_root=$(CDPATH= cd "$script_dir/../.." && pwd -P)
tmp_root="$repo_root/.work/tmp"
export TMPDIR="$tmp_root"
export CARGO_TARGET_DIR="$repo_root/.work/bench-target"
if [ "$#" -lt 3 ] || [ "$#" -gt 5 ] || [ "$#" -eq 4 ]; then
    printf '%s\n' 'usage: measure.sh ROOT DEVICE:INODE WORKLOAD [CROSS_ROOT CROSS_DEVICE:INODE]' >&2
    exit 2
fi
features_ready=$(env | awk -F= '$1 == "ISH_FS_BENCH_FEATURES_READY" { print $2 }')
if [ "$features_ready" != 1 ]; then
    printf '%s\n' 'bench.sh: local P3 samples are locked until features and available P2 gates are ready' >&2
    exit 2
fi
git -C "$repo_root" check-ignore -q .work/tmp || {
    printf '%s\n' 'measure.sh: .work/tmp is not ignored by .git/info/exclude' >&2
    exit 2
}

fixture_root=$1
fixture_identity=$2
workload=$3
cross_root=
cross_identity=
if [ "$#" -eq 5 ]; then
    cross_root=$4
    cross_identity=$5
fi
case "$workload" in
    all|tiny-copy|tiny-move|flat|nested|deep|wide|large|clone|sparse|hardlinks|rename|cross|merge|progress-on|progress-off) ;;
    *) printf 'measure.sh: unknown workload %s\n' "$workload" >&2; exit 2 ;;
esac

run_helper() {
    cargo run --quiet -p ish-fs --example fs_bench -- "$@"
}

run_runner() {
    "$runner_binary" --ignored --exact "$1" --nocapture
}

verify_sample() {
    selected_workload=$1
    selected_side=$2
    sample_name=$3
    if [ -n "$cross_root" ]; then
        export ISH_FS_BENCH_CROSS_ROOT=$cross_root
        export ISH_FS_BENCH_CROSS_IDENTITY=$cross_identity
    else
        unset ISH_FS_BENCH_CROSS_ROOT ISH_FS_BENCH_CROSS_IDENTITY
    fi
    export ISH_FS_BENCH_ROOT=$fixture_root
    export ISH_FS_BENCH_IDENTITY=$fixture_identity
    export ISH_FS_BENCH_WORKLOAD=$selected_workload
    export ISH_FS_BENCH_SIDE=$selected_side
    export ISH_FS_BENCH_SAMPLE=$sample_name
    run_runner fs_bench_verify || return $?
    run_runner fs_bench_cleanup || return $?
}

cp_path=$(command -v cp || true)
mv_path=$(command -v mv || true)
cp_version=$(cp --version 2>&1 | head -n 2 || true)
cp_help=$(cp --help 2>&1 || true)
cp_clone_mode=none
case "$cp_version" in
    *'GNU coreutils'*) case "$cp_help" in *--reflink*) cp_clone_mode=gnu ;; esac ;;
    *) case "$(uname -s)" in Darwin) cp_clone_mode=apple ;; esac ;;
esac
case "$cp_clone_mode" in
    gnu) native_cp_label=gnu-coreutils-cp; native_clone_label=gnu-coreutils-cp-reflink-auto ;;
    apple) native_cp_label=apple-cp; native_clone_label=apple-cp-clone-flag-c ;;
    *)
        case "$cp_version" in *BusyBox*) native_cp_label=busybox-cp ;; *) native_cp_label=unclassified-cp ;; esac
        native_clone_label=unavailable
        ;;
esac
if [ -n "$mv_path" ]; then native_mv_label="$(basename "$mv_path")-mv-durability-unverified"; else native_mv_label=unavailable; fi

if [ -n "$cross_root" ]; then
    run_helper verify "$fixture_root" "$fixture_identity" "$cross_root" "$cross_identity"
else
    run_helper verify "$fixture_root" "$fixture_identity"
fi
"$script_dir/bench.sh" host "$fixture_root"
if PERF_GATE_LANE_GIB=4 perf-gate-ready -s "$repo_root"; then :; else exit $?; fi

build_json=$(mktemp "$tmp_root/fs-bench-build.XXXXXX")
build_time=$(mktemp "$tmp_root/fs-bench-build-time.XXXXXX")
results_file=$(mktemp "$tmp_root/fs-bench-results.XXXXXX")
sample_file=
setup_file=
cleanup_files() {
    for path in "$build_json" "$build_time" "$sample_file" "$setup_file"; do
        if [ -n "$path" ]; then rm -f "$path"; fi
    done
}
remove_sample_files() {
    if [ -n "$sample_file" ]; then rm -f "$sample_file" || return $?; fi
    if [ -n "$setup_file" ]; then rm -f "$setup_file" || return $?; fi
    sample_file=
    setup_file=
}
trap cleanup_files 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
busy_checks=0
if PERF_GATE_ATTEMPTS=3 PERF_GATE_WAIT=0 PERF_GATE_LANE_GIB=4 perf-gate-ready; then
    :
else
    gate_status=$?
    if [ "$gate_status" -eq 75 ]; then
        printf '%s\n' 'perf gate deferred: host busy' >&2
        exit 75
    fi
    exit "$gate_status"
fi
if /usr/bin/time -p cargo test --locked --release -p ish --test fs_bench --no-run --message-format=json >"$build_json" 2>"$build_time"; then
    build_status=0
else
    build_status=$?
fi
if PERF_GATE_LANE_GIB=4 perf-gate-ready -1; then build_gate_status=0; else build_gate_status=$?; fi
if [ "$build_status" -ne 0 ]; then
    cat "$build_time" >&2
    printf '%s\n' 'fs-bench: release harness build failed' >&2
    exit "$build_status"
fi
build_times=$(awk '$1 == "real" { real = $2 } $1 == "user" { user = $2 } $1 == "sys" { system_seconds = $2 } END { printf "elapsed=%s user_cpu=%s system_cpu=%s", real, user, system_seconds }' "$build_time")
if [ "$build_gate_status" -eq 0 ]; then
    build_gate_label=valid
elif [ "$build_gate_status" -eq 75 ]; then
    build_gate_label=discarded-overlap
    busy_checks=$((busy_checks + 1))
else
    printf '%s\n' 'fs-bench: perf-gate-ready could not read load after release build' >&2
    exit "$build_gate_status"
fi
printf 'fs_bench_build kind=release-test-runner gate=%s %s\n' "$build_gate_label" "$build_times"
runner_binary=$(jaq -r 'select(.reason == "compiler-artifact" and .target.name == "fs_bench" and .executable != null) | .executable' "$build_json" | tail -n 1)
[ -x "$runner_binary" ] || {
    printf '%s\n' 'fs-bench: Cargo did not report the fs_bench test executable' >&2
    exit 1
}

run_sample() {
    selected_workload=$1
    selected_side=$2
    round=$3
    sample_name="$selected_workload-r$round-$selected_side"
    sample_file="$tmp_root/fs-bench-sample-$$-$round-$selected_side.out"
    setup_file="$tmp_root/fs-bench-setup-$$-$round-$selected_side.out"
    case "$selected_side" in
        native-cp) utility_label=$native_cp_label; export ISH_FS_BENCH_CP=$cp_path; unset ISH_FS_BENCH_CLONE_MODE ;;
        native-clone) utility_label=$native_clone_label; export ISH_FS_BENCH_CP=$cp_path; export ISH_FS_BENCH_CLONE_MODE=$cp_clone_mode ;;
        native-mv) utility_label=$native_mv_label; export ISH_FS_BENCH_MV=$mv_path; unset ISH_FS_BENCH_CLONE_MODE ;;
        *) utility_label=none; unset ISH_FS_BENCH_CLONE_MODE ;;
    esac
    export ISH_FS_BENCH_UTILITY_LABEL=$utility_label
    export ISH_FS_BENCH_ROOT=$fixture_root
    export ISH_FS_BENCH_IDENTITY=$fixture_identity
    export ISH_FS_BENCH_WORKLOAD=$selected_workload
    export ISH_FS_BENCH_SIDE=$selected_side
    export ISH_FS_BENCH_SAMPLE=$sample_name
    unset ISH_FS_BENCH_CROSS_ROOT ISH_FS_BENCH_CROSS_IDENTITY
    if [ -n "$cross_root" ]; then
        export ISH_FS_BENCH_CROSS_ROOT=$cross_root
        export ISH_FS_BENCH_CROSS_IDENTITY=$cross_identity
    fi
    if run_runner fs_bench_prepare_sample >"$setup_file" 2>&1; then :; else
        setup_status=$?
        cat "$setup_file" >&2
        return "$setup_status"
    fi
    if PERF_GATE_ATTEMPTS=1 PERF_GATE_WAIT=0 PERF_GATE_LANE_GIB=4 perf-gate-ready; then
        :
    else
        gate_status=$?
        if [ "$gate_status" -eq 75 ]; then
            busy_checks=$((busy_checks + 1))
            printf 'fs-bench: skipped-before-sample workload=%s side=%s busy_checks=%s\n' "$selected_workload" "$selected_side" "$busy_checks" >&2
            run_runner fs_bench_cleanup || return $?
            remove_sample_files || return $?
            if [ "$busy_checks" -ge 3 ]; then printf '%s\n' 'perf gate deferred: host busy' >&2; return 75; fi
            return 0
        fi
        return "$gate_status"
    fi
    if run_runner fs_bench_sample >"$sample_file" 2>&1; then sample_status=0; else sample_status=$?; fi
    if PERF_GATE_LANE_GIB=4 perf-gate-ready -1; then after_status=0; else after_status=$?; fi
    if [ "$sample_status" -ne 0 ]; then cat "$sample_file" >&2; return "$sample_status"; fi
    case "$after_status" in
        0) ;;
        75)
            busy_checks=$((busy_checks + 1))
            printf 'fs-bench: discarded-overlap workload=%s side=%s busy_checks=%s\n' "$selected_workload" "$selected_side" "$busy_checks" >&2
            verify_sample "$selected_workload" "$selected_side" "$sample_name" || return $?
            remove_sample_files || return $?
            if [ "$busy_checks" -ge 3 ]; then printf '%s\n' 'perf gate deferred: host busy' >&2; return 75; fi
            return 0
            ;;
        *) cat "$sample_file" >&2; return "$after_status" ;;
    esac
    verify_sample "$selected_workload" "$selected_side" "$sample_name" || return $?
    cat "$sample_file" || return $?
    rg '^fs_bench_sample ' "$sample_file" >> "$results_file" || return $?
    remove_sample_files || return $?
}

measure_workload() {
    selected_workload=$1
    if [ "$selected_workload" = cross ] && [ -z "$cross_root" ]; then
        printf '%s\n' 'fs-bench: cross workload unavailable; no distinct-device root was designated below .work/tmp'
        return 0
    fi
    case "$selected_workload" in
        tiny-move|rename|cross) side_list='engine builtin native-mv' ;;
        *) side_list='engine builtin native-cp' ;;
    esac
    case "$selected_workload:$cp_clone_mode" in
        clone:gnu|clone:apple|large:gnu|large:apple|sparse:gnu|sparse:apple) side_list="$side_list native-clone" ;;
    esac
    if [ -z "$cp_path" ]; then side_list=$(printf '%s\n' "$side_list" | sed 's/ native-cp//; s/ native-clone//'); fi
    if [ -z "$mv_path" ]; then side_list=$(printf '%s\n' "$side_list" | sed 's/ native-mv//'); fi
    reversed=
    for selected_side in $side_list; do reversed="$selected_side $reversed"; done
    round=0
    while :; do
        round=$((round + 1))
        complete=1
        if [ $((round % 2)) -eq 1 ]; then ordered_sides=$side_list; else ordered_sides=$reversed; fi
        for selected_side in $ordered_sides; do
            accepted=$(awk -v workload_name="$selected_workload" -v side_name="$selected_side" '$1 == "fs_bench_sample" && $2 == "workload=" workload_name && $3 == "side=" side_name { count++ } END { print count + 0 }' "$results_file")
            if [ "$accepted" -ge 5 ]; then continue; fi
            if run_sample "$selected_workload" "$selected_side" "$round"; then :; else return $?; fi
            accepted=$(awk -v workload_name="$selected_workload" -v side_name="$selected_side" '$1 == "fs_bench_sample" && $2 == "workload=" workload_name && $3 == "side=" side_name { count++ } END { print count + 0 }' "$results_file")
            if [ "$accepted" -lt 5 ]; then complete=0; fi
        done
        if [ "$complete" -eq 1 ]; then break; fi
    done
}

if [ "$workload" = all ]; then
    for selected_workload in tiny-copy tiny-move flat nested deep wide large clone sparse hardlinks rename cross merge progress-on progress-off; do
        if measure_workload "$selected_workload"; then :; else exit $?; fi
    done
else
    if measure_workload "$workload"; then :; else exit $?; fi
fi

awk '
    function median(values, count) {
        if (count % 2) return values[(count + 1) / 2]
        return (values[count / 2] + values[count / 2 + 1]) / 2
    }
    function sort(values, total, i, prior, value) {
        for (i = 2; i <= total; i++) {
            value = values[i]
            prior = i - 1
            while (prior >= 1 && values[prior] > value) { values[prior + 1] = values[prior]; prior-- }
            values[prior + 1] = value
        }
    }
    {
        if ($1 != "fs_bench_sample") next
        for (i = 2; i <= NF; i++) { split($i, pair, "="); fields[pair[1]] = pair[2] }
        group = fields["workload"] ":" fields["side"]
        count[group]++
        elapsed[group, count[group]] = fields["elapsed_seconds"] + 0
        cpu[group, count[group]] = fields["user_cpu_seconds"] + fields["system_cpu_seconds"]
    }
    END {
        for (group in count) {
            runs = count[group]
            for (i = 1; i <= runs; i++) { elapsed_values[i] = elapsed[group, i]; cpu_values[i] = cpu[group, i] }
            sort(elapsed_values, runs)
            sort(cpu_values, runs)
            printf "fs_bench_summary workload_side=%s accepted_runs=%d median_elapsed_seconds=%.9f elapsed_range_seconds=%.9f..%.9f min_cpu_seconds=%.6f median_cpu_seconds=%.6f cpu_range_seconds=%.6f..%.6f\n", group, runs, median(elapsed_values, runs), elapsed_values[1], elapsed_values[runs], cpu_values[1], median(cpu_values, runs), cpu_values[1], cpu_values[runs]
            if (runs != 5) printf "fs_bench_summary status=incomplete_required_runs=5_accepted_runs=%d workload_side=%s\n", runs, group
        }
    }
' "$results_file"
printf 'fs_bench_results_file=%s\n' "$results_file"
