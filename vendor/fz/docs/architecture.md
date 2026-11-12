# Architecture

`src/main.rs` retains the standalone process entry point. `src/cli.rs` parses options, reads candidate bytes, and chooses interactive or filter mode. `src/lib.rs` stores candidates and ranks matches. `src/score.rs` implements fzy matching, scoring, and highlight positions. `src/terminal.rs` owns the raw terminal session and rendering.

## Matching and ranking

Eligibility is a byte-wise subsequence search. Lowercase ASCII query bytes accept either case; uppercase query bytes accept only uppercase. Other bytes match exactly. Scoring folds ASCII case after eligibility, as fzy does. Matching does not normalize or case-fold Unicode.

Exact matches score positive infinity. Empty queries, nonmatches, and candidates longer than 1024 bytes score negative infinity. Candidates over 1024 bytes remain eligible, but they have no highlighted positions. Equal scores retain input order.

The upstream C build can round the initial gap score differently by platform: the macOS Clang build contracts the expression when FMA is available, while upstream's Linux GCC build rounds it twice. Preserve those target-specific defaults because a one-ULP difference can change ranking.

`Choices` ranks only the result prefix needed by the screen and expands that prefix as rows are read. The library type defaults to one worker; the CLI requests automatic sizing, capped at four, and small searches remain serial. The workers share candidate storage and combine disjoint result ranges before ranking.

## Terminal and process behavior

Interactive mode reads candidates from stdin and opens the configured terminal device, `/dev/tty` by default. The terminal saves its original termios state, enters raw mode, and restores the saved state on completion, cancellation, or drop. Linux resize delivery uses a blocked `SIGWINCH` mask and `signalfd`; it does not install a signal handler. macOS waits with `select` because `poll` reports `POLLNVAL` for `/dev/tty`. `SIGWINCH` stays blocked, and a thread blocked in `sigwait` writes one byte to a pipe; a handler does not run, because handling `SIGWINCH` during a tty operation can wedge the process. A bare Escape cancels 25ms of wall time after the byte, so a resize wake cannot postpone that deadline.

Result rows wrap at the terminal width instead of truncating, so a long candidate occupies as many physical rows as it needs within the `--lines` budget; later candidates are dropped from view when the budget is full. Unlike fzy, which clips a row at the right edge, wrapping keeps the tail of entries such as long process argument lists readable on narrow terminals. Wide and combining characters count by display width when wrapping.

The binary uses a custom entry point to skip Rust runtime startup. It keeps the default `SIGPIPE` disposition and flushes its own stdout before returning.

## Embedding

`Picker` owns query editing, focus, marking order, and `Choices`. It returns
`Outcome::Accepted` with stable `CandidateId` values, `Query`, `NoMatch`, or
`Cancelled`. Duplicate payloads retain separate IDs. Retrieve payload bytes
with `Picker::source`. The caller frames selected output.

`Picker::feed`, `paste`, `resize`, `render`, and `clear` have no process or
terminal ownership effects. The host supplies events and a writer, checks
resize errors before acceptance, and restores its terminal before using IDs.
`PickerConfig::workers` defaults are caller policy. Use one worker when embedding
in a process that later forks. Search workers are scoped and joined.

`cli::run` implements the executable contract for a fresh worker process. Its
standalone terminal adapter owns termios and resize delivery. Do not call that
adapter over an existing host event loop. The library installs no panic hook
and never exits the process. Custom process entry and SIGPIPE assumptions stay
in `main.rs`.

`find_either` and `has_match` also support borrowed byte corpora. The host can
retain its own ranking and Unicode policy without invoking fzy scoring or its
1024-byte highlight limit.
