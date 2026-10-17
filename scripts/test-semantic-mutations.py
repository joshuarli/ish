from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-semantic-mutations.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("check_semantic_mutations", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {SCRIPT}")
MUTATIONS = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MUTATIONS
SPEC.loader.exec_module(MUTATIONS)


class SemanticMutationRunnerTest(unittest.TestCase):
    def test_explicit_lean_executable_can_live_outside_a_lake_layout(self) -> None:
        environment = os.environ.copy()
        MUTATIONS.ignored_work_directory(environment)
        scratch_root = ROOT / ".work/tmp"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="mutation-lean-path-", dir=scratch_root) as scratch:
            directory = Path(scratch)
            binary = directory / "standalone" / "chosen-bridge"
            binary.parent.mkdir()
            binary.write_text("#!/bin/sh\nprintf chosen-bridge")
            binary.chmod(0o755)
            project = directory / "project"
            (project / "formal").mkdir(parents=True)
            MUTATIONS.install_lean_binary(project, binary)
            result = MUTATIONS.checked(
                [str(project / "formal/.lake/build/bin/semanticBridge")],
                project,
                environment,
            )
            self.assertEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "chosen-bridge")

    def test_timeout_terminates_descendant_process_group(self) -> None:
        environment = os.environ.copy()
        MUTATIONS.ignored_work_directory(environment)
        scratch_root = ROOT / ".work/tmp"
        scratch_root.mkdir(parents=True, exist_ok=True)

        with tempfile.TemporaryDirectory(prefix="mutation-timeout-", dir=scratch_root) as scratch:
            marker = Path(scratch) / "descendant-terminated"
            child_source = "\n".join(
                (
                    "import os, signal, sys",
                    f"marker = {str(marker)!r}",
                    "def terminate(signum, frame):",
                    "    with open(marker, 'w', encoding='utf-8') as output:",
                    "        output.write('terminated')",
                    "    raise SystemExit(0)",
                    "signal.signal(signal.SIGTERM, terminate)",
                    "os.write(int(sys.argv[1]), b'ready')",
                    "signal.pause()",
                )
            )
            parent_source = "\n".join(
                (
                    "import os, signal, subprocess, sys",
                    "read_fd, write_fd = os.pipe()",
                    "child = subprocess.Popen(",
                    "    [sys.executable, '-c', sys.argv[1], str(write_fd)],",
                    "    pass_fds=(write_fd,),",
                    ")",
                    "os.close(write_fd)",
                    "if os.read(read_fd, 5) != b'ready':",
                    "    raise RuntimeError('descendant did not become ready')",
                    "os.close(read_fd)",
                    "print(child.pid, flush=True)",
                    "signal.pause()",
                )
            )

            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as failure:
                    MUTATIONS.checked(
                        [sys.executable, "-c", parent_source, child_source],
                        ROOT,
                        environment,
                        timeout=2,
                    )

            self.assertEqual(failure.exception.code, 1)
            self.assertEqual(marker.read_text(encoding="utf-8"), "terminated")


if __name__ == "__main__":
    unittest.main()
