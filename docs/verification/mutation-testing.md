# Run semantic bridge mutations

Use the mutation runner to check that the semantic bridge detects changes in production behavior.

1. If Lean has not built `semanticBridge`, run `cd formal && lake build semanticBridge`.
2. From the repository root, run `python3 scripts/check-semantic-mutations.py`.

The runner copies the current tracked and untracked source into a temporary project under `.work/tmp`. It builds the baseline, runs each family check, then compiles and runs one production mutant at a time. A mutant counts as killed only when its compiled bridge exits at the expected semantic assertion. Build errors, timeouts, and unrelated failures fail the runner.

The mutations cover division overflow in `crates/epsh/src/arith.rs`, adjacent IFS empty fields in `crates/epsh/src/expand.rs`, dynamic command refusal before redirections in `crates/epsh/src/eval.rs`, and saved descriptor restoration in `crates/epsh/src/redirect.rs`.

The bridge's family comparator controls still alter normalized outcomes in memory. They check the comparison path. The mutation runner compiles changed production source in the isolated project.

The runner needs Python 3.10 or later, Cargo, and a built Lean executable in the current worktree. Pass `--lean-binary PATH` when the executable is elsewhere. Before writing scratch files, it adds `.work/` to the local Git exclude file when needed.
