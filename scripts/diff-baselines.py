#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import re
import shutil
import textwrap
from pathlib import Path


ANSI_RE = re.compile(r"\033\[[0-9;]*m")
RESET = "\033[0m"
RED = "\033[31m"
GREEN = "\033[32m"


def read_baseline(path: Path) -> dict[str, tuple[float, float | None, float]]:
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


def format_delta(candidate: float | None, baseline: float | None, colors: bool) -> str:
    if candidate is None:
        return "removed"
    if baseline is None:
        return "new"
    if baseline == 0:
        change = 0.0 if candidate == 0 else None
    else:
        change = (candidate - baseline) / baseline * 100
    if change is None:
        return "new"
    value = f"{change:+.2f}%"
    if colors and change != 0:
        color = GREEN if change < 0 else RED
        return f"{color}{value}{RESET}"
    return value


def visible_width(value: str) -> int:
    return len(ANSI_RE.sub("", value))


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

    def wrap(value: str, width: int) -> list[str]:
        plain = ANSI_RE.sub("", value)
        return textwrap.wrap(
            plain,
            width=max(1, width),
            break_long_words=True,
            break_on_hyphens=False,
            replace_whitespace=False,
            drop_whitespace=True,
        ) or [""]

    def print_row(values: tuple[str, ...]) -> None:
        wrapped = [wrap(value, width) for value, width in zip(values, widths)]
        for line_number in range(max(map(len, wrapped))):
            cells = [
                (lines[line_number] if line_number < len(lines) else "").ljust(width)
                for lines, width in zip(wrapped, widths)
            ]
            print("│" + "│".join(f" {cell} " for cell in cells) + "│")

    print(border("╭", "┬", "╮"))
    print_row(headers)
    print(border("├", "┼", "┤"))
    for row in rows:
        print_row(row)
    print(border("╰", "┴", "╯"))


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare two benchmark baselines")
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()

    for path in (args.baseline, args.candidate):
        if not path.is_file():
            parser.error(f"baseline not found: {path}")

    baseline = read_baseline(args.baseline)
    candidate = read_baseline(args.candidate)
    colors = os.environ.get("NO_COLOR") is None and os.isatty(1)
    rows = []
    for name in sorted(set(baseline) | set(candidate)):
        old = baseline.get(name)
        new = candidate.get(name)
        old_median, old_counts, old_allocations = old or (None, None, None)
        new_median, new_counts, new_allocations = new or (None, None, None)
        rows.append(
            (
                name,
                f"{format_time(new_median)} ({format_delta(new_median, old_median, colors)})"
                if new_median is not None
                else "removed",
                f"{format_bytes(new_allocations)} ({format_delta(new_allocations, old_allocations, colors)})"
                if new_allocations is not None
                else "removed",
                f"{new_counts:.0f} allocs/op ({format_delta(new_counts, old_counts, colors)})"
                if new_counts is not None
                else "removed",
            )
        )

    print(f"{args.baseline} → {args.candidate}")
    print_table(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
