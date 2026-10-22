#!/usr/bin/env python3
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


RELEASE = "nightly-2026.10.17-e62dd83"
RUSTUP_VERSION = "1.29.1"
EXTRACTOR_TOOLCHAIN = "nightly-2026-09-17"
CACHE_MARKER = RELEASE + "\n" + RUSTUP_VERSION + "\n" + EXTRACTOR_TOOLCHAIN + "\n"
TOOL_SHA256 = {
    ("aeneas", "x86_64"): "377d030171f423987acc9ccba1d2e6117b8bfd34053f5e8628d5aa70ceae7b9c",
    ("aeneas", "aarch64"): "486909db8ff48a5c8400c5aaaa34044319ffa9f601df78f4e9391d2ebd8cd898",
    ("rustup", "x86_64"): "dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71",
    ("rustup", "aarch64"): "15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433",
}
ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / ".work/tools" / RELEASE
SCRATCH = ROOT / ".work/tmp"
ENV = dict(os.environ, TMPDIR=str(SCRATCH), MATHLIB_NO_CACHE_ON_UPDATE="1",
           MATHLIB_CACHE_DIR=str(SCRATCH / "mathlib-cache"))


def run(args, *, cwd=ROOT, env=ENV):
    print("+", " ".join(map(str, args)), flush=True)
    return subprocess.run(list(map(str, args)), cwd=cwd, env=env, check=True)


def download_verified(url, destination, artifact, expected):
    if not expected:
        raise SystemExit(f"No pinned SHA-256 for {artifact}")
    print("Downloading", url, flush=True)
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as temporary:
        temporary_path = Path(temporary.name)
        try:
            with urllib.request.urlopen(url) as source:
                shutil.copyfileobj(source, temporary)
            temporary.flush()
            temporary.seek(0)
            actual = hashlib.file_digest(temporary, "sha256").hexdigest()
            if actual != expected:
                raise SystemExit(f"SHA-256 mismatch for {artifact}: expected {expected}, got {actual}")
            os.replace(temporary_path, destination)
        finally:
            temporary_path.unlink(missing_ok=True)


def prepare():
    exclude = Path(subprocess.check_output(
        ["git", "rev-parse", "--git-path", "info/exclude"], cwd=ROOT, text=True).strip())
    if not exclude.is_absolute():
        exclude = ROOT / exclude
    if not exclude.exists() or ".work/" not in exclude.read_text().splitlines():
        exclude.parent.mkdir(parents=True, exist_ok=True)
        with exclude.open("a") as rules:
            rules.write("\n.work/\n")
    run(["git", "check-ignore", "--quiet", ".work/"])
    SCRATCH.mkdir(parents=True, exist_ok=True)
    TOOLS.parent.mkdir(parents=True, exist_ok=True)
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise SystemExit("The pinned prebuilt extraction tools require Linux x86_64 or aarch64")
    complete = TOOLS / ".complete"
    if TOOLS.exists() and (not complete.is_file() or complete.read_text() != CACHE_MARKER):
        shutil.rmtree(TOOLS)
    if not TOOLS.exists():
        with tempfile.TemporaryDirectory(dir=TOOLS.parent) as temporary:
            temporary = Path(temporary)
            archive = temporary / "aeneas.tar.gz"
            unpacked = temporary / "unpacked"
            machine = platform.machine()
            download_verified(
                f"https://github.com/AeneasVerif/aeneas/releases/download/{RELEASE}/"
                f"aeneas-linux-{machine}.tar.gz",
                archive,
                f"Aeneas {RELEASE} linux-{machine}.tar.gz",
                TOOL_SHA256.get(("aeneas", machine)),
            )
            with tarfile.open(archive) as bundle:
                bundle.extractall(unpacked, filter="data")
            version = subprocess.check_output([str(unpacked / "aeneas"), "-version"], text=True).strip()
            if version != "aeneas " + RELEASE:
                raise SystemExit(f"Expected Aeneas {RELEASE}, found {version}")
            unpacked.rename(TOOLS)
    version = subprocess.check_output([str(TOOLS / "aeneas"), "-version"], text=True).strip()
    if version != "aeneas " + RELEASE:
        raise SystemExit(f"Expected Aeneas {RELEASE}, found {version}")
    backend = TOOLS / "backends/lean"
    if not (backend / ".lake/packages/mathlib/lakefile.lean").exists():
        run(["lake", "update"], cwd=backend)
    modules = set()
    for source in backend.glob("Aeneas*/**/*.lean"):
        for line in source.read_text().splitlines():
            if line.startswith(("import Mathlib", "public import Mathlib", "public meta import Mathlib",
                                "import Batteries", "public import Batteries", "public meta import Batteries")):
                modules.add(line.split("import ", 1)[1].split()[0])
    search = [backend / ".lake/build/lib/lean"]
    search.extend(package / ".lake/build/lib/lean" for package in (backend / ".lake/packages").iterdir())
    if any(not any((base / (module.replace(".", "/") + ".olean")).exists() for base in search)
           for module in modules):
        run(["lake", "exe", "cache", "get", *sorted(modules)], cwd=backend)
    return search


def charon(args):
    native = platform.libc_ver()[0] == "glibc"
    rustenv = dict(ENV, RUSTUP_HOME=str(TOOLS / "rustup"), CARGO_HOME=str(TOOLS / "cargo"),
                   RUSTUP_TOOLCHAIN=EXTRACTOR_TOOLCHAIN,
                   PATH=str(TOOLS / "cargo/bin") + os.pathsep + ENV["PATH"])
    prefix = []
    if not native:
        prefix = ["docker", "run", "--rm", "--user", f"{os.getuid()}:{os.getgid()}",
                  "-v", f"{ROOT}:{ROOT}", "-w", str(ROOT)]
        for key in ("RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_HOME", "TMPDIR"):
            prefix.extend(["-e", f"{key}={rustenv[key]}"])
        prefix.extend(["-e", f"PATH={TOOLS}/cargo/bin:/usr/bin:/bin", "debian:bookworm-slim"])
    if not (TOOLS / "rustup/toolchains" / (EXTRACTOR_TOOLCHAIN + "-" + platform.machine() + "-unknown-linux-gnu")).exists():
        installer = TOOLS / "rustup-init"
        if not installer.exists():
            machine = platform.machine()
            download_verified(
                f"https://static.rust-lang.org/rustup/archive/{RUSTUP_VERSION}/"
                f"{machine}-unknown-linux-gnu/rustup-init",
                installer,
                f"rustup {RUSTUP_VERSION} {machine}-unknown-linux-gnu/rustup-init",
                TOOL_SHA256.get(("rustup", machine)),
            )
            installer.chmod(0o755)
        run([*prefix, installer, "-y", "--no-modify-path", "--profile", "minimal",
             "--default-toolchain", EXTRACTOR_TOOLCHAIN, "--component", "rustc-dev,rust-src,llvm-tools"],
            env=rustenv)
    run([*prefix, TOOLS / "charon", "rustc", "--preset", "aeneas", "--sysroot", "default", *args],
        env=rustenv)


def main():
    search = prepare()
    run(["lake", "build", "IshSemantics.Arithmetic"], cwd=ROOT / "formal")
    with tempfile.TemporaryDirectory(prefix="refinement-", dir=SCRATCH) as temporary:
        output = Path(temporary)
        crate = output / "epsh_shift.rs"
        source = ROOT / "crates/epsh/src/arith/shift.rs"
        rust_path = str(source).replace("\\", "\\\\").replace('"', '\\"')
        crate.write_text(f'#[path = "{rust_path}"]\nmod shift;\n')
        charon(["--dest-file", output / "epsh_shift.llbc", "--", "--crate-type", "lib",
                "--crate-name", "epsh_shift", "--emit=metadata", "-o", output / "epsh_shift.rmeta", crate])
        run([TOOLS / "aeneas", "-backend", "lean", "-use-lean-modules", "false",
             "-dest", output, output / "epsh_shift.llbc"])
        leanenv = dict(ENV, LEAN_PATH=os.pathsep.join(map(str, [output, *search, ROOT / "formal/.lake/build/lib/lean"])))
        run(["lean", "--root=" + str(output), "-o", output / "EpshShift.olean", output / "EpshShift.lean"],
            cwd=ROOT / "formal", env=leanenv)
        run(["lean", ROOT / "formal/Refinement/Shift.lean"], cwd=ROOT / "formal", env=leanenv)
    marker = TOOLS / ".complete"
    temporary_marker = TOOLS / ".complete.tmp"
    temporary_marker.write_text(CACHE_MARKER)
    os.replace(temporary_marker, marker)
    print("Production shift-count extraction, refinement, and axiom audit passed", flush=True)


if __name__ == "__main__":
    main()
