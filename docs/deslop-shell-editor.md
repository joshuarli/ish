# Shell and editor deslop evidence

Scope: ish `src/`, split between execution and persistence (DSLP-3/4/5) and editor and utilities (DSLP-6/11/16). Both are consumed by the ish binary and PTY tests. Measurements are Darwin arm64, default features, Rust 1.102.0-nightly (5b7b37aef 2026-11-08), base ffd27b855bd7262b4ee45e91673b0ba64f2508a9. Linux-specific branches are not measured here.

## Shell execution and persistence: types (DSLP-3)

`cargo clippy -p ish --lib --bins -- -W unsafe_code -W dead_code -W unreachable_pub -W clippy::wildcard_enum_match_arm -W clippy::match_wildcard_for_single_variants -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::unwrap_used -W clippy::expect_used -W clippy::redundant_clone -W clippy::needless_pass_by_value` exited 0. Counts are diagnostics with primary location in the component, not proven defects: unsafe 27, wildcard enum 3, single-variant wildcard 0, truncation 17, sign loss 2, unwrap 9, expect 33, redundant clone 0, needless pass by value 3. `unreachable_pub`: 5; see dead-code audit. Counts include separate lib and bin compilations where applicable. The component has 6,828 Rust lines and 306 matching lines under the deslop escape-hatch expression, including tests.

No candidate is adopted: no candidate has a demonstrated defect witness or removed escape hatch, and several flag intentional behavior. For example, `src/dispatch.rs:216` returns `None` for any non-static word, `src/history.rs:149` asserts the store just opened, and `src/signal.rs:387` writes a signal number to the signal pipe. These examples do not prove all remaining diagnostics false positives. Existing FFI makes `unsafe_code = forbid` inapplicable. No type contract or runtime code changed; escape-hatch count is unchanged.
