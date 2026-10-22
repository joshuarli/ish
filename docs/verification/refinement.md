# Shift-count refinement

The production helper in [`arith/shift.rs`](../../crates/epsh/src/arith/shift.rs)
accepts counts from 0 through 63 and converts them to `u32`. The arithmetic
parser uses this helper for both shift operators and preserves its existing
error for other counts.

[`Shift.lean`](../../formal/Refinement/Shift.lean) proves two universal statements
about the freshly extracted helper:

- `shift_count_refines` proves that every `i64` input returns the correct domain
  decision and a lossless count conversion, without an extraction failure.
- `shift_count_matches_model` connects that result to both existing Lean shift
  branches for every 64-bit left operand.

The check extracts the actual production file through Charon and Aeneas. Lean
checks the generated definition and the proof in a temporary directory. Each run
regenerates the definition, so a stale translation cannot satisfy the check.
The proof audits its transitive axioms against `propext`, `Quot.sound`, and
`Classical.choice`. Admitting count 64 makes the check fail.

## Check requirements

Run `make verify-refinement` with Lean 4.35.0 and Python 3.12 or later on `PATH`.
The pinned official tools support Linux x86_64 and ARM64. On musl hosts, the
checker runs Charon in a Debian container and requires Docker.
Lean commands run from `formal/` so Elan finds the project pin without a global
default toolchain.

The checker downloads [Aeneas nightly-2026.10.17-e62dd83](https://github.com/AeneasVerif/aeneas/releases/tag/nightly-2026.10.17-e62dd83)
and its matching Charon under `.work/tools/`. It verifies the pinned SHA-256
for the Linux x86_64 or ARM64 archive before extraction. Both tools use Apache
2.0 licenses. The extractor needs a separate Rust nightly-2026-09-17 toolchain
under that directory, installed by verified rustup 1.29.1 GNU-host installers.
The production Rust pin remains nightly-2026-09-15. Existing tool caches are
reused only after a completed refinement run; older incomplete caches are
discarded and rebuilt from the verified downloads.

The main bundle is about 153.4 MiB. A fresh installed tool tree occupies about
3.75 GiB, including GNU Rust, the Aeneas Lean backend, and mathlib sources and
compiled caches. The compressed mathlib cache uses another 0.1 GiB.
A clean bootstrap duration has not been measured. Ordinary
Cargo builds do not run these tools or add their dependencies to the shell.

## Trust and scope

The proof trusts rustc's MIR extraction, Charon, and Aeneas to translate the Rust
source correctly. Lean's kernel checks the correspondence of that translation
to the model. The extractor uses its own compiler version, and this is not a
proof about the machine code emitted by the production compiler.

The proof covers count validation and conversion. Production shift primitives,
other arithmetic operators, parsing, and evaluator behavior remain outside its
scope. The differential bridge and Rust tests continue to check those paths.
