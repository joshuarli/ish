# Terminal interoperability matrix (ISH-80)

Standing acceptance requirement for terminal behavior after ISH-73 capture removal and ISH-74 OSC 133 integration. Deterministic PTY coverage is automated; native terminal/multiplexer observations are recorded separately. Unavailable native environments are explicitly unverified and are not counted as passes.

Host recorded: macOS 27.2 (`Darwin ... arm64`), `xterm-256color` session, tmux 3.8, Ghostty 1.4.1-main+42f2367 installed at `/Applications/Ghostty Custom.app/Contents/MacOS/ghostty`, SSH localhost unavailable (`Connection refused`, port 22).

## Deterministic PTY coverage (`cargo test --test pty`, ptytest)

| Area | Coverage | Evidence |
|---|---|---|
| Simple commands | covered | existing PTY tests; `echo_command`, `osc133_marks_prompt_input_execution_and_completion` |
| Interactive pipeline/list members | partial | existing PTY/native-fs/list/pipeline refusal coverage; pipeline/list `/dev/tty` behavior remains outside the single-command converged form |
| `/dev/tty` input (converged terminal) | covered (single command) | `converged_command_reads_from_dev_tty` reads input from `/dev/tty` through the converged slave |
| Fullscreen/alternate-screen programs | covered (relay/pass-through, bounded) | `alternate_screen_sequences_pass_through_relay` verifies alt-screen control sequences transit the relay and the shell remains responsive; full emulator screen-state behavior remains native/manual |
| Terminal replies and reports (DA, CPR, focus, mouse, OSC/DCS/APC/PM/SOS, CSI-u) | covered | `reply_and_report_sequences_are_consumed_whole_at_every_split` and the other `input::tests` split-framing tests; `child_reporting_modes_and_query_replies_never_reach_the_next_prompt_line` sends replies after a child exits. Real SSH, tmux, and Ghostty splits are unverified. |
| Multiline/bracketed paste | covered | `multiline_continuation`, `bracketed_paste_*`, `osc133_continuation_does_not_emit_extra_execution_markers` |
| Resize storms | covered (bounded) | `resize_storm_keeps_shell_responsive` |
| Ctrl+C/Ctrl+Z | covered | existing PTY tests; `ctrl_c_cancels_input`, suspend/fg tests, OSC 133 cancel/stop tests |
| Repeated stop/resume | covered (bounded) | `repeated_stop_resume_preserves_command_boundaries` |
| EOF/hangup | partial | Ctrl+D/exit covered; `shell_sighup_at_prompt_exits_and_restores_terminal` and native-fs SIGTERM/SIGHUP cases cover explicit shell signal handling; external child hangup/EOF edge cases and native SIGHUP/terminal-loss behavior remain partially unverified |
| Redirections | covered | existing PTY tests; `osc133_redirected_output_keeps_markers_off_command_file` |
| Prompt markers (OSC 133) | covered | `osc133_*` tests: order/status, cancel/refusal, resize/continuation, suspend/fg |
| OSC 7/52 behavior | covered (bounded) | `osc7_working_directory_emitted_to_interactive_terminal`; `osc52_copy_scrollback_emits_base64_session_log` asserts raw OSC 52 clipboard output with the session-log payload |
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

Bounded automated checks added: resize storm responsiveness, repeated stop/resume command boundaries, `/dev/tty` single-command input, alt-screen relay passthrough responsiveness, OSC 52 raw emission, and SIGHUP-at-prompt exit/restore. Remaining stress checks (leaked descriptors/children, hung waits, lost input, terminal modes not restored across native environments, and external child hangup/EOF edge cases) need focused instrumentation and native runs; record outcomes explicitly, never as implicit passes.
