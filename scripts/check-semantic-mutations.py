#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import shutil
import signal
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BRIDGE_TIMEOUT = 120
BUILD_TIMEOUT = 300
PROCESS_GROUP_GRACE = 2
PROCESS_GROUP_DRAIN_TIMEOUT = 5


@dataclass(frozen=True)
class Mutation:
    name: str
    family: str
    cases: int
    expected_failure: str
    edits: tuple[tuple[str, str, str], ...]


MUTATIONS = (
    Mutation(
        name="arithmetic division overflow",
        family="arithmetic",
        cases=3,
        expected_failure="mismatch at arithmetic-00002",
        edits=(
            (
                "crates/epsh/src/arith.rs",
                '''                if left == i64::MIN && right == -1 {
                    return Err("division overflow".into());
                }
                left / right''',
                "                left.wrapping_div(right)",
            ),
        ),
    ),
    Mutation(
        name="adjacent IFS delimiter empty field",
        family="expansion",
        cases=368,
        expected_failure="mismatch at expansion-",
        edits=(
            (
                "crates/epsh/src/expand.rs",
                "fields.push(String::new()); // empty field",
                "current.clear();",
            ),
        ),
    ),
    Mutation(
        name="dynamic command check after redirection setup",
        family="policy",
        cases=2,
        expected_failure="runtime policy hook did not reject eval before assignments and redirections",
        edits=(
            (
                "crates/epsh/src/eval.rs",
                "        self.check_command_policy_for_dispatch(&expanded_args, span)?;\n",
                "",
            ),
            (
                "crates/epsh/src/eval.rs",
                (
                    "        let saved_fds = self.setup_redirections(redirs)?;\n\n"
                    "        let result = match self.try_builtin(cmd_name, &expanded_args, assigns, &[], span) {\n"
                ),
                (
                    "        let saved_fds = self.setup_redirections(redirs)?;\n"
                    "        self.check_command_policy_for_dispatch(&expanded_args, span)?;\n\n"
                    "        let result = match self.try_builtin(cmd_name, &expanded_args, assigns, &[], span) {\n"
                ),
            ),
        ),
    ),
    Mutation(
        name="saved descriptor restoration",
        family="effects",
        cases=11,
        expected_failure="mismatch at effects-00010",
        edits=(
            (
                "crates/epsh/src/redirect.rs",
                "match sys::dup2_result(copy, s.target_fd) {",
                "match sys::close(s.target_fd) {",
            ),
        ),
    ),
)


def checked(
    command: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: int = BRIDGE_TIMEOUT,
) -> subprocess.CompletedProcess[str]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        cleanup_errors: list[str] = []
        cleanup_error = signal_process_group(process.pid, signal.SIGTERM)
        if cleanup_error is not None:
            cleanup_errors.append(cleanup_error)
        try:
            stdout, stderr = process.communicate(timeout=PROCESS_GROUP_GRACE)
        except subprocess.TimeoutExpired:
            cleanup_error = signal_process_group(process.pid, signal.SIGKILL)
            if cleanup_error is not None:
                cleanup_errors.append(str(cleanup_error))
            try:
                stdout, stderr = process.communicate(timeout=PROCESS_GROUP_DRAIN_TIMEOUT)
            except subprocess.TimeoutExpired:
                try:
                    process.kill()
                except OSError as cleanup_error:
                    cleanup_errors.append(str(cleanup_error))
                try:
                    stdout, stderr = process.communicate(timeout=PROCESS_GROUP_DRAIN_TIMEOUT)
                except subprocess.TimeoutExpired:
                    stdout = ""
                    stderr = ""
                cleanup_errors.append("process group did not close its output pipes after SIGKILL")
        cleanup_error = signal_process_group(process.pid, signal.SIGKILL)
        if cleanup_error is not None:
            cleanup_errors.append(cleanup_error)
        if isinstance(stdout, bytes):
            stdout = stdout.decode(errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        cleanup_detail = f"; process-group cleanup errors: {'; '.join(cleanup_errors)}" if cleanup_errors else ""
        fail(
            f"subprocess timed out after {timeout}s: {' '.join(command)}{cleanup_detail}",
            subprocess.CompletedProcess(command, 124, stdout, stderr),
        )
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def signal_process_group(process_group_id: int, signal_number: int) -> str | None:
    try:
        os.killpg(process_group_id, signal_number)
    except ProcessLookupError:
        return None
    except OSError as error:
        return str(error)
    return None


def output_of(result: subprocess.CompletedProcess[str]) -> str:
    return "\n".join(part for part in (result.stdout, result.stderr) if part).strip()


def fail(message: str, result: subprocess.CompletedProcess[str] | None = None) -> None:
    print(message, file=sys.stderr)
    if result is not None:
        output = output_of(result)
        if output:
            print(output, file=sys.stderr)
    raise SystemExit(1)


def ignored_work_directory(env: dict[str, str]) -> None:
    exclude_result = checked(["git", "rev-parse", "--git-path", "info/exclude"], ROOT, env)
    if exclude_result.returncode != 0:
        fail("cannot locate .git/info/exclude", exclude_result)
    exclude_path = Path(exclude_result.stdout.strip())
    if not exclude_path.is_absolute():
        exclude_path = ROOT / exclude_path
    contents = exclude_path.read_text() if exclude_path.exists() else ""
    if ".work/" not in contents.splitlines():
        exclude_path.parent.mkdir(parents=True, exist_ok=True)
        with exclude_path.open("a") as exclude_file:
            if contents and not contents.endswith("\n"):
                exclude_file.write("\n")
            exclude_file.write(".work/\n")
    result = checked(["git", "check-ignore", "-v", ".work/"], ROOT, env)
    if result.returncode != 0 or ".git/info/exclude" not in result.stdout:
        fail("could not ensure .work is ignored through .git/info/exclude", result)


def discover_lean_binary(env: dict[str, str], explicit: Path | None) -> Path:
    if explicit is not None:
        candidate = explicit.expanduser().resolve()
        if candidate.is_file():
            return candidate
        fail(f"Lean executable not found at {candidate}")

    candidate = ROOT / "formal/.lake/build/bin/semanticBridge"
    if candidate.is_file():
        return candidate.resolve()
    fail(
        "Lean executable is missing from this worktree; run `cd formal && lake build semanticBridge` "
        "or pass --lean-binary"
    )


def copy_working_tree(destination: Path, env: dict[str, str]) -> None:
    result = checked(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        ROOT,
        env,
    )
    if result.returncode != 0:
        fail("cannot list the current working tree", result)
    for raw_path in result.stdout.encode().split(b"\0"):
        if not raw_path:
            continue
        relative = Path(os.fsdecode(raw_path))
        if relative.parts[0] in {".git", ".work", "target"}:
            continue
        source = ROOT / relative
        if not source.exists() and not source.is_symlink():
            continue
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.symlink_to(os.readlink(source), target_is_directory=source.is_dir())
        elif source.is_file():
            shutil.copy2(source, target)


def install_lean_binary(project: Path, lean_binary: Path) -> None:
    lake_artifacts = project / "formal/.lake"
    if lake_artifacts.exists() or lake_artifacts.is_symlink():
        fail(f"unexpected Lean artifact path in isolated snapshot: {lake_artifacts}")
    executable = lake_artifacts / "build/bin/semanticBridge"
    executable.parent.mkdir(parents=True)
    executable.symlink_to(lean_binary)


def cargo_build(project: Path, target: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return checked(
        [
            "cargo",
            "build",
            "--quiet",
            "--manifest-path",
            str(project / "Cargo.toml"),
            "-p",
            "ish",
            "--example",
            "semantic_bridge",
        ],
        project,
        {**env, "CARGO_TARGET_DIR": str(target)},
        timeout=BUILD_TIMEOUT,
    )


def bridge_run(
    project: Path,
    target: Path,
    env: dict[str, str],
    family: str,
    cases: int,
) -> subprocess.CompletedProcess[str]:
    binary = target / "debug/examples/semantic_bridge"
    return checked(
        [str(binary), "--family", family, "--cases", str(cases)],
        project,
        env,
        timeout=BRIDGE_TIMEOUT,
    )


def apply_mutation(project: Path, mutation: Mutation) -> dict[Path, str]:
    originals: dict[Path, str] = {}
    for relative, before, after in mutation.edits:
        path = project / relative
        originals.setdefault(path, path.read_text())
        current = path.read_text()
        if current.count(before) != 1:
            fail(f"{mutation.name}: expected one source match in {relative}")
        path.write_text(current.replace(before, after, 1))
    return originals


def restore_mutation(originals: dict[Path, str]) -> None:
    for path, contents in originals.items():
        path.write_text(contents)


def main() -> None:
    parser = argparse.ArgumentParser(description="Run isolated production mutations against the semantic bridge")
    parser.add_argument("--lean-binary", type=Path)
    arguments = parser.parse_args()

    cargo_bin = Path.home() / ".cargo/bin"
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join((str(cargo_bin), env.get("PATH", "")))
    ignored_work_directory(env)
    workspace_tmp = ROOT / ".work/tmp"
    workspace_tmp.mkdir(parents=True, exist_ok=True)
    lean_binary = discover_lean_binary(env, arguments.lean_binary)

    with tempfile.TemporaryDirectory(prefix="semantic-mutations-", dir=workspace_tmp) as scratch_name:
        scratch = Path(scratch_name)
        project = scratch / "project"
        project.mkdir()
        target = scratch / "target"
        temporary = scratch / "tmp"
        temporary.mkdir()
        env["TMPDIR"] = str(temporary)
        copy_working_tree(project, env)
        (project / ".work/tmp").mkdir(parents=True, exist_ok=True)
        install_lean_binary(project, lean_binary)

        baseline_build = cargo_build(project, target, env)
        if baseline_build.returncode != 0:
            fail("baseline semantic bridge did not compile", baseline_build)
        print("baseline build: passed")

        for mutation in MUTATIONS:
            baseline = bridge_run(project, target, env, mutation.family, mutation.cases)
            if baseline.returncode != 0:
                fail(f"baseline {mutation.family} semantic verification failed", baseline)
            print(f"baseline {mutation.family}: passed")

        for mutation in MUTATIONS:
            originals = apply_mutation(project, mutation)
            try:
                mutant_build = cargo_build(project, target, env)
                if mutant_build.returncode != 0:
                    fail(f"{mutation.name}: mutant did not compile; compile errors are not kills", mutant_build)
                mutant = bridge_run(project, target, env, mutation.family, mutation.cases)
                output = output_of(mutant)
                if mutant.returncode == 0 or mutation.expected_failure not in output:
                    fail(
                        f"{mutation.name}: semantic bridge did not detect the compiled mutant",
                        mutant,
                    )
                failure_line = next(
                    line.strip() for line in output.splitlines() if mutation.expected_failure in line
                )
                print(f"killed {mutation.name}: {failure_line}")
            finally:
                restore_mutation(originals)

    print(f"mutation run: {len(MUTATIONS)} compiled mutants killed by semantic assertions")


if __name__ == "__main__":
    main()
