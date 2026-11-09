# Terminal interoperability matrix (ISH-80)

Standing acceptance requirement for terminal behavior after ISH-73 capture removal and ISH-74 OSC 133 integration. Deterministic PTY coverage is automated; native terminal/multiplexer observations are recorded separately. Unavailable native environments are explicitly unverified and are not counted as passes.

Host recorded: macOS 27.2 (`Darwin ... arm64`), `xterm-256color` session, tmux 3.8, Ghostty 1.4.1-main+42f2367 installed at `/Applications/Ghostty Custom.app/Contents/MacOS/ghostty`, SSH localhost unavailable (`Connection refused`, port 22).

## Deterministic PTY coverage (`cargo test --test pty`, ptytest)

| Area | Coverage | Evidence |
|---|---|---|
| Simple commands | covered | existing PTY tests; `echo_command`, `osc133_marks_prompt_input_execution_and_completion` |
| Interactive pipeline/list members | partial | existing PTY/native-fs/list/pipeline refusal coverage; direct `/dev/tty` input case not yet isolated |
| `/dev/tty` input (converged terminal) | unverified by isolated test | architecture claims converged child `/dev/tty` resolves to slave; add focused PTY/native evidence in follow-up |
| Fullscreen/alternate-screen programs | unverified | requires representative program fixture and screen-state assertions |
| Multiline/bracketed paste | covered | `multiline_continuation`, `bracketed_paste_*`, `osc133_continuation_does_not_emit_extra_execution_markers` |
| Resize storms | covered (bounded) | `resize_storm_keeps_shell_responsive` |
| Ctrl+C/Ctrl+Z | covered | existing PTY tests; `ctrl_c_cancels_input`, suspend/fg tests, OSC 133 cancel/stop tests |
| Repeated stop/resume | covered (bounded) | `repeated_stop_resume_preserves_command_boundaries` |
| EOF/hangup | partial | Ctrl+D/exit covered; explicit external hangup/SIGHUP matrix not fully isolated |
| Redirections | covered | existing PTY tests; `osc133_redirected_output_keeps_markers_off_command_file` |
| Prompt markers (OSC 133) | covered | `osc133_*` tests: order/status, cancel/refusal, resize/continuation, suspend/fg |
| OSC 7/52 behavior | partial | `osc7_working_directory_emitted_to_interactive_terminal`; OSC 52 not asserted in PTY (profile/harness constraints) |
| Delegated login-shell command mode | covered | `login_shell` tests; `delegated_ish_c_emits_no_osc133_markers` |

## Native/manual observations

| Environment | Status | Observation |
|---|---|---|
| Ghostty 1.4.1-main | unverified (manual) | Binary/app installed on host. Automated GUI prompt-navigation/output-boundary checks not run; verify prompt navigation, command/output boundaries, and notification/completion behavior where supported. |
| Independent terminal emulator | unverified | Not automated in this session. |
| SSH session | unverified | Localhost SSH unavailable on this host; test on a host with SSH access. |
| tmux 3.8 | unverified (inconclusive) | tmux available. Automated detached/capture smoke attempt was inconclusive in this environment; run interactive/manual verification for prompt markers, resize, paste, stop/resume, and marker passthrough/wrapping behavior. |
| macOS native host (PTY) | covered (automated PTY) | Focused PTY suite and ISH-74/80 additions passed on macOS arm64 host during this change. |
| Linux musl CI | not run locally | CI workflow runs Linux musl lint/test; check CI results for this change. |

## Stress/boundary checks

Bounded automated checks added: resize storm responsiveness and repeated stop/resume command boundaries. Remaining stress checks (leaked descriptors/children, hung waits, lost input, terminal modes not restored across native environments) need focused instrumentation and native runs; record outcomes explicitly, never as implicit passes.
