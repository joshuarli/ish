#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path


TIME_RE = re.compile(r"([0-9]+(?:\.[0-9]+)?)\s*(ns|µs|ms|s)")
COUNT_RE = re.compile(r"^\s*(?:│\s*)?([0-9]+(?:\.[0-9]+)?)\s+│")
BYTES_RE = re.compile(r"^\s*(?:│\s*)?([0-9]+(?:\.[0-9]+)?)\s+(B|KB|MB|GB)\s+│")
ALLOC_RE = re.compile(r"^\s*(?:│\s*)?alloc:\s+│")
BENCH_RE = re.compile(r"^[├╰]─\s+(\S+)")
ANSI_RE = re.compile(r"\033\[[0-9;]*m")

RESET = "\033[0m"
RED = "\033[31m"
GREEN = "\033[32m"


def sysctl(name: str) -> str:
    return subprocess.check_output(["sysctl", "-n", name], text=True).strip()


def host_info() -> tuple[str, int]:
    if platform.system() != "Darwin":
        raise SystemExit("bench-baseline.py currently supports macOS only")

    brand = sysctl("machdep.cpu.brand_string")
    model = next((f"m{i}" for i in (1, 2, 3, 4) if f"M{i}" in brand), "intel")
    cpus = int(sysctl("hw.ncpu"))
    memory_gb = (int(sysctl("hw.memsize")) + (1 << 30) - 1) // (1 << 30)
    host = f"mac-{model}-{cpus}-{memory_gb}gb"
    return host, cpus


def time_ns(value: float, unit: str) -> float:
    return value * {"ns": 1, "µs": 1_000, "ms": 1_000_000, "s": 1_000_000_000}[unit]


def byte_count(value: float, unit: str) -> float:
    return value * {"B": 1, "KB": 1024, "MB": 1 << 20, "GB": 1 << 30}[unit]


def parse_report(output: str) -> dict[str, tuple[float, float, float]]:
    values: dict[str, tuple[float, float, float]] = {}
    current: str | None = None
    median: float | None = None
    allocation_count = 0.0
    allocation_bytes = 0.0
    allocation_state = 0

    def flush() -> None:
        if current is not None and median is not None:
            values[current] = (median, allocation_count, allocation_bytes)

    for line in output.splitlines():
        match = BENCH_RE.match(line)
        if match:
            flush()
            current = match.group(1)
            median = None
            allocation_count = 0.0
            allocation_bytes = 0.0
            allocation_state = 0
            pairs = TIME_RE.findall(line)
            if len(pairs) >= 3:
                median = time_ns(float(pairs[2][0]), pairs[2][1])
            continue

        if ALLOC_RE.match(line):
            allocation_state = 1
            continue
        if allocation_state == 1:
            match = COUNT_RE.match(line)
            if match:
                allocation_count = float(match.group(1))
            allocation_state = 2
            continue
        if allocation_state == 2:
            match = BYTES_RE.match(line)
            if match:
                allocation_bytes = byte_count(float(match.group(1)), match.group(2))
            allocation_state = 0
            continue

        if current is not None and median is None:
            pairs = TIME_RE.findall(line)
            if len(pairs) >= 3:
                median = time_ns(float(pairs[2][0]), pairs[2][1])

    flush()
    return values


def read_baseline(path: Path) -> dict[str, tuple[float, float | None, float]]:
    if not path.exists():
        return {}
    values: dict[str, tuple[float, float | None, float]] = {}
    for line in path.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        columns = line.split("\t")
        if len(columns) == 3:
            name, median, allocations = columns
            values[name] = (float(median), None, float(allocations))
        else:
            name, median, counts, allocations = columns
            values[name] = (float(median), float(counts), float(allocations))
    return values


def write_baseline(path: Path, host: str, values: dict[str, tuple[float, float, float]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as file:
            file.write("# ish benchmark baseline\n")
            file.write(f"# host: {host}\n")
            file.write("# columns: benchmark median_ns alloc_count alloc_bytes\n")
            for name in sorted(values):
                median, counts, allocations = values[name]
                file.write(f"{name}\t{median:.0f}\t{counts:.0f}\t{allocations:.0f}\n")
        os.replace(temporary, path)
    except BaseException:
        os.unlink(temporary)
        raise


def format_time(value: float) -> str:
    if value < 1_000:
        return f"{value:.0f} ns/op"
    if value < 1_000_000:
        return f"{value / 1_000:.2f} µs/op"
    if value < 1_000_000_000:
        return f"{value / 1_000_000:.2f} ms/op"
    return f"{value / 1_000_000_000:.2f} s/op"


def format_bytes(value: float) -> str:
    if value < 1024:
        return f"{value:.0f} B/op"
    if value < 1024 * 1024:
        return f"{value / 1024:.2f} KB/op"
    if value < 1024 * 1024 * 1024:
        return f"{value / (1024 * 1024):.2f} MB/op"
    return f"{value / (1024 * 1024 * 1024):.2f} GB/op"


def format_delta(current: float, previous: float | None, colors: bool) -> str:
    if previous is None:
        return "new"
    if previous == 0:
        return "0.00%" if current == 0 else "new"
    change = (current - previous) / previous * 100
    text = f"{change:+.2f}%"
    if not colors or change == 0:
        return text
    color = GREEN if change < 0 else RED
    return f"{color}{text}{RESET}"


def benchmark_name(name: str) -> str:
    return "Benchmark" + "".join(part[:1].upper() + part[1:] for part in name.split("_"))


def visible_width(value: str) -> int:
    return len(ANSI_RE.sub("", value))


def pad_cell(value: str, width: int) -> str:
    return value + " " * (width - visible_width(value))


def wrap_cell(value: str, width: int) -> list[str]:
    plain = ANSI_RE.sub("", value)
    lines = textwrap.wrap(
        plain,
        width=max(1, width),
        break_long_words=True,
        break_on_hyphens=False,
        replace_whitespace=False,
        drop_whitespace=True,
    ) or [""]
    for color in (RED, GREEN):
        if color not in value:
            continue
        start = value.index(color) + len(color)
        end = value.index(RESET, start)
        colored = value[start:end]
        lines = [line.replace(colored, f"{color}{colored}{RESET}") for line in lines]
    return lines


def print_table(rows: list[tuple[str, str, str, str]]) -> None:
    headers = ("benchmark", "time", "memory", "allocs/op")
    preferred = [
        max(visible_width(row[index]) for row in rows + [headers])
        for index in range(len(headers))
    ]
    minimum = [12, 12, 12, 12]
    terminal_width = shutil.get_terminal_size((120, 24)).columns
    available = max(4 * len(headers), terminal_width - (3 * len(headers) + 1))
    if available < sum(minimum):
        base, remainder = divmod(available, len(headers))
        widths = [base + (index < remainder) for index in range(len(headers))]
    else:
        widths = minimum[:]
        while sum(widths) < available:
            candidates = [
                index for index in range(len(headers)) if widths[index] < preferred[index]
            ]
            if not candidates:
                break
            index = max(candidates, key=lambda candidate: preferred[candidate] - widths[candidate])
            widths[index] += 1

    def border(left: str, join: str, right: str) -> str:
        return left + join.join("─" * (width + 2) for width in widths) + right

    def row(values: tuple[str, ...], line_number: int) -> str:
        cells = []
        for value, width in zip(values, widths):
            lines = wrap_cell(value, width)
            cells.append(pad_cell(lines[line_number] if line_number < len(lines) else "", width))
        cells = [f" {cell} " for cell in cells]
        return "│" + "│".join(cells) + "│"

    def print_row(values: tuple[str, ...]) -> None:
        wrapped = [wrap_cell(value, width) for value, width in zip(values, widths)]
        for line_number in range(max(map(len, wrapped))):
            print(row(values, line_number))

    print(border("╭", "┬", "╮"))
    print_row(headers)
    print(border("├", "┼", "┤"))
    for values in rows:
        print_row(values)
    print(border("╰", "┴", "╯"))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--baseline",
        type=Path,
        help="baseline path to read and write instead of the host default",
    )
    parser.add_argument(
        "--variant",
        help="suffix for the host default baseline path, such as pgo",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="update the baseline without printing the comparison table",
    )
    parser.add_argument(
        "--print-path",
        action="store_true",
        help="print the selected baseline path and exit",
    )
    return parser.parse_args()


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    args = parse_args()
    host, cpus = host_info()
    suffix = f"-{args.variant}" if args.variant else ""
    baseline_path = args.baseline or root / "benches" / f"{host}{suffix}-baseline.txt"
    if args.print_path:
        print(baseline_path)
        return 0
    completed = subprocess.run(
        ["cargo", "bench", "--bench", "bench"],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if completed.returncode:
        print(completed.stdout, end="")
        return completed.returncode

    current = parse_report(completed.stdout)
    if not current:
        print("ish: could not parse Divan benchmark output", file=sys.stderr)
        print(completed.stdout, end="", file=sys.stderr)
        return 1

    previous = read_baseline(baseline_path)
    colors = sys.stdout.isatty() and "NO_COLOR" not in os.environ
    rows = []
    for name in sorted(current):
        median, counts, allocations = current[name]
        old_median, old_counts, old_allocations = previous.get(name, (None, None, None))
        time_change = format_delta(median, old_median, colors)
        memory_change = format_delta(allocations, old_allocations, colors)
        alloc_change = format_delta(counts, old_counts, colors)
        rows.append(
            (
                benchmark_name(name) + "-" + str(cpus),
                f"{format_time(median)} ({time_change})",
                f"{format_bytes(allocations)} ({memory_change})",
                f"{counts:.0f} allocs/op ({alloc_change})",
            )
        )

    if not args.quiet:
        print_table(rows)
    write_baseline(baseline_path, host, current)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
