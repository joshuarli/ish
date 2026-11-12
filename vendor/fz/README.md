# fz

`fz` is a small Rust port of John Hawthorn's **fzy** fuzzy selector. It reads candidates from stdin, lets you filter and choose one in your terminal, and writes the selection to stdout.

## Install

Download a prerelease from [GitHub Releases](https://github.com/laputa-systems/fz/releases). Each release publishes `SHA256SUMS` and two static Linux archives:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `fz-x86_64-unknown-linux-musl-static.zip` |
| Linux arm64 | `fz-aarch64-unknown-linux-musl-static.zip` |

The release workflow builds macOS arm64 and does not publish it. Build that binary from a checkout; see [Release](docs/release.md).

Extract an archive and put its executable on your `PATH`:

```sh
mkdir -p ~/.local/bin
unzip fz-x86_64-unknown-linux-musl-static.zip
install -m 755 fz-x86_64-unknown-linux-musl-static ~/.local/bin/fz
```

Add `~/.local/bin` to `PATH` if it is not already there.

## Use

Pipe candidates into `fz` for interactive selection:

```sh
printf 'alpha\nbeta\n' | fz
```

By default, input records are separated by newlines and empty records are skipped; carriage returns are preserved. Enter writes the selected candidate followed by a newline. If nothing matches, Enter writes the query. Escape or Ctrl-C, Ctrl-D, or Ctrl-G cancels with status 1 and no stdout.

Use `--show-matches` to filter without opening a terminal:

```sh
fz --show-matches=mr < paths.txt
```

This prints matching candidates in score order and exits with status 0, including when there are no matches. Ties keep input order. `--read-null` reads NUL-separated records, which can contain newlines; selected output still ends with a newline.

## Options

| Option | Effect |
| --- | --- |
| `-l`, `--lines=LINES` | Number of result rows (default 10; minimum 3; or `max`) |
| `-p`, `--prompt=PROMPT` | Prompt text (default `> `) |
| `-q`, `--query=QUERY` | Start with a query |
| `-e`, `--show-matches=QUERY` | Print sorted matches without a terminal |
| `-t`, `--tty=TTY` | Terminal device (default `/dev/tty`) |
| `-s`, `--show-scores` | Show match scores |
| `-0`, `--read-null` | Read NUL-separated candidates |
| `-j`, `--workers=NUM` | Maximum workers (default up to four; small searches use one) |
| `-i`, `--show-info` | Show the match count |
| `-m`, `--multi` | Mark multiple results with Space, then accept with Enter |
| `-h`, `--help` | Show help |
| `-v`, `--version` | Show version |

Up/Down or Ctrl-P/N (also Ctrl-K/J) moves through results. Left/Right, Home/End, and Ctrl-A/E move the query cursor. Backspace deletes a character, Ctrl-W deletes the preceding word, Ctrl-U deletes before the cursor, Tab completes the selected candidate, and PageUp/PageDown moves by a page.

Matching follows fzy's byte-oriented behavior: lowercase ASCII query letters match either case, uppercase letters match uppercase only, and non-ASCII bytes match exactly. Query editing moves by UTF-8 code point; matching does not normalize or case-fold Unicode.

### Multi-select

Use `fz --multi --query=needle` to mark multiple candidates in one terminal session.
Type to filter, then press **Down** to focus the first result. In the result list,
**Space** marks or unmarks the focused row, and Up/Down moves between rows.
**Up** from the first result returns to query editing. Typing any non-space
printable character in the result list also returns to query editing.
The selected count is always visible; marks persist when the query changes.
**Enter** outputs every marked result in marking order, one line per result.
With no marks, Enter accepts the focused result as usual; with no matches
it outputs nothing and exits with status 1. **Esc** cancels without output
(status 1). Terminal or I/O failures in multi-select mode return status 2;
a terminal too small to display selectable rows cannot accept a selection.
Space in query editing still inserts a space. This mode is opt-in; ordinary
single-selection fz behavior remains unchanged.

Multi-select output is newline-delimited, like normal interactive output.
Avoid `--read-null` with embedded-newline records if the consumer needs
unambiguous output boundaries. `--multi` cannot be combined with `--show-matches`.

For help, use `fz --help` or visit the [issue tracker](https://github.com/laputa-systems/fz/issues). The project is MIT licensed; see [LICENSE](LICENSE).
