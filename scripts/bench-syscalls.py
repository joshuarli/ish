#!/usr/bin/env python3

from __future__ import annotations

import json
import re
import shlex
import subprocess
import sys


IMAGE = "benchmark-syscalls:local"
WORKDIR = "/workspace"
SAMPLE_COUNT = 10
SAMPLE_SIZE = 100
BEGIN_MARKER = 'prctl(PR_SET_NAME, "BENCH_BEGIN"'
END_MARKER = 'prctl(PR_SET_NAME, "BENCH_END"'
SYSCALL_RE = re.compile(r"(?:\[pid \d+\] )?([A-Za-z_][A-Za-z0-9_]*)\(")


def run(command: list[str], *, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, check=True, text=True, capture_output=capture)


def docker_run(command: str, *, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return run(
        [
            "docker",
            "run",
            "--rm",
            "--cap-add=SYS_PTRACE",
            "--security-opt",
            "seccomp=unconfined",
            "-v",
            f"{subprocess.check_output(['pwd'], text=True).strip()}:{WORKDIR}",
            "-w",
            WORKDIR,
            IMAGE,
            "sh",
            "-lc",
            command,
        ],
        capture=capture,
    )


def build_image() -> None:
    run(["docker", "build", "--quiet", "-t", IMAGE, "."], capture=True)


def benchmark_executable() -> str:
    result = docker_run("cargo bench --bench bench --no-run --message-format=json", capture=True)
    executable = None
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "bench":
            executable = message.get("executable")
    if not executable:
        raise RuntimeError("could not find Divan benchmark executable")
    return executable


def benchmark_names(executable: str) -> list[str]:
    result = docker_run(f"{shlex.quote(executable)} --bench --list", capture=True)
    pattern = re.compile(r"^[├╰]─\s+(\S+)")
    return [match.group(1) for line in result.stdout.splitlines() if (match := pattern.match(line))]


def trace_benchmark(executable: str, name: str) -> list[tuple[str, int, int]]:
    command = (
        f"SYSCALL_TRACE=1 strace -f -qq -o /tmp/strace-events "
        f"{shlex.quote(executable)} --bench {shlex.quote(name)} "
        f"--sample-count {SAMPLE_COUNT} --sample-size {SAMPLE_SIZE} >/dev/null 2>&1; "
        "cat /tmp/strace-events"
    )
    result = docker_run(command, capture=True)
    totals: dict[str, list[int]] = {}
    active = False
    for line in result.stdout.splitlines():
        if BEGIN_MARKER in line:
            active = True
            continue
        if END_MARKER in line:
            active = False
            continue
        if not active:
            continue
        match = SYSCALL_RE.search(line)
        if not match or " = " not in line:
            continue
        syscall = match.group(1)
        tally = totals.setdefault(syscall, [0, 0])
        tally[0] += 1
        if re.search(r" = -1(?:\s|$)", line):
            tally[1] += 1
    return [(syscall, calls, errors) for syscall, (calls, errors) in totals.items()]


def print_report(results: dict[str, list[tuple[str, int, int]]]) -> None:
    headers = ("syscall", "calls", "errors")
    iterations = SAMPLE_COUNT * SAMPLE_SIZE

    for benchmark, syscalls in results.items():
        rows = sorted(syscalls, key=lambda row: (-row[1], row[0]))
        values = [
            (syscall, str(calls), str(errors))
            for syscall, calls, errors in rows
        ]
        widths = [
            max([len(header)] + [len(row[index]) for row in values])
            for index, header in enumerate(headers)
        ]

        def border(left: str, join: str, right: str) -> str:
            return left + join.join("─" * (width + 2) for width in widths) + right

        def line(row: tuple[str, ...], *, header: bool = False) -> str:
            cells = []
            for index, (value, width) in enumerate(zip(row, widths)):
                alignment = "<" if header or index == 0 else ">"
                cells.append(f" {value:{alignment}{width}} ")
            return "│" + "│".join(cells) + "│"

        print(f"{benchmark} ({iterations} iterations)")
        print(border("╭", "┬", "╮"))
        print(line(headers, header=True))
        print(border("├", "┼", "┤"))
        for row in values:
            print(line(row))
        print(border("╰", "┴", "╯"))
        print()


def main() -> int:
    build_image()
    executable = benchmark_executable()
    names = benchmark_names(executable)
    results = {name: trace_benchmark(executable, name) for name in names}
    print_report(results)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
