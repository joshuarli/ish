I'd like to build an extremely lightweight and minimal interactive shell. I've enjoyed using ~/dev/fish-shell, but I don't need most of its feature set. There are some features I really admire though and would like to have:

- history search is extremely fast and supports fuzzy search. Not sure how it's powered - my thought would be sqlite + fts5 but hopefully it's a much lighter weight implementation. I think sqlite for a shell might be too much.
  - i'm also impressed with the UX (how it displays history entries and lets you cycle through - we should replicate this experience exactly)
- in ~/dev/fish-shell i've added a native prompt built into the shell at src/reader/native_prompt.rs. We should port this over.
- i really like fish's tab autocomplete when it lists files, it prints dir entries in a neat grid and you can navigate through them with all arrow keys.
- i like fish's autoexpansion of aliases
- i'd like extremely basic job control - just ^Z and `fg` is enough, supporting only a single job. No support for `&`.

My constraints:
- No TUI, just use raw mode and assume vt100.
- Absolute minimal dependencies.
- No need for any POSIX compatibility; this is purely an interactive shell, it won't be used to run any scripts at all. In fact we should refuse to do so. And we refuse to "source" anything as well.
- Cannot understate the minimalism here; we should not support anything like while/for, etc. We don't run scripts, we are only in charge of composing commands for exec.
- Single config file to be read on each startup at ~/.config/ish/config.ish. We ignore everything else, even ~/.profile.
  - This config file only supports:
    - set ENV_VAR_NAME "string"
      - example: set PATH "$HOME/usr/bin"
    - alias alias_name arg1 arg2 argv
      - example: alias tmux -f "$XDG_CONFIG_HOME/tmux/tmux.conf" new -s
- No ability to customize prompt; the prompt is native and built into the shell.
- No need for ability to extend autocomplete dynamically / load additional functions/files at runtime. Everything I need will be compiled in.
- Tab autocomplete is dumb, it always assumes you want to pick a file. I know fish's autocmoplete is sophisticated but 99% of the time I jsut need to pick a file.
- No need for any of fish's on-the-fly style for invalid commands and the like.

Let's plan this out in great and thorough detail before implementing.
I'm probably forgetting to mention some basic features I take for granted in using fish-shell.

As for the core of the shell itself, I would take inspiration from absolute minimal shells out there - I think https://github.com/emersion/mrsh is a shining example. I wouldn't read bash's source code which is like 170K+ lines of C.

---

core features:

 Command execution:
  - Pipes: cmd1 | cmd2 | cmd3
  - Chaining: && (run if success), || (run if failure), ; (run regardless)
  - Redirections: >, >>, <, 2>, 2>&1
  - Command substitution: $(cmd) — capture stdout, substitute inline
  - Glob expansion: * (any chars), ? (single char) — shell expands before
  exec
  - Tilde expansion: ~ → $HOME
  - Variable expansion: $VAR in unquoted and double-quoted contexts
  - Quoting: "double with $expansion", 'single literal', \ escape

  Line editing (all standard keybinds):
  - Left/Right arrows — move cursor
  - Home / Ctrl+A — beginning of line
  - End / Ctrl+E — end of line
  - Ctrl+Left / Alt+B — move word backward
  - Ctrl+Right / Alt+F — move word forward
  - Backspace — delete char before cursor
  - Delete / Ctrl+D — delete char at cursor (or exit on empty line)
  - Ctrl+W — delete word backward
  - Ctrl+U — delete to beginning of line
  - Ctrl+K — delete to end of line
  - Ctrl+Y — yank (paste last deleted text)
  - Ctrl+C — cancel current line, fresh prompt
  - Ctrl+L — clear screen

  History:
  - Saved to ~/.local/share/ish/history, persists across sessions
  - Up/Down — cycle full history (empty line) or prefix search (with text)
  - Ctrl+R — fuzzy subsequence search with pager UI (type to filter, Up/Down
  to cycle matches, Enter to accept, Escape to cancel)
  - Deduplication (only most recent instance kept)

 Tab completion:
  - File/directory completion only (always)
  - Grid display (column-major, like fish)
  - Arrow key navigation through grid
  - Directories shown with trailing /
  - Enter/Tab to accept, Escape to cancel

  Prompt:
  - user@host colored_pwd [*] [branch] $
  - PWD: ~ for home, middle dirs abbreviated to 1 char (~/d/fish-shell)
  - Color: green on success, red on last command failure
  - Git branch from .git/HEAD with smart caching
  - Red * when __DENV_DIRTY=1

  Aliases:
  - Defined in config: alias name cmd [args...]
  - Autoexpansion: typing the alias name and pressing space visually expands
  it inline
  - User args appended after alias expansion

  Config (~/.config/ish/config.ish):
  - set VAR "value" — set environment variable
  - alias name cmd [args...] — define alias
  - Read on each startup, nothing else

  Builtins:
  - cd [dir] / cd - (previous directory)
  - exit
  - fg — resume suspended job
  - set VAR "value" — set env var at runtime
  - alias name cmd [args...] — define alias at runtime
  - history — list/clear history

  Job control:
  - Ctrl+Z — suspend foreground process
  - fg — resume it
  - Single job slot only, no &

  Safety:
  - Refuses to run script files (args on command line → error)
  - Refuses to source anything
  - No while/for/if/function — not recognized as keywords

---

i should also mention that our shell should not be subprocessing out
to anything, period. If we need to list a dir, we implement it ourselves.

we should additionally implement:
- &|
- i can't remember 2>&1; should be &>. Keep 2> though.
- glob should also support things like */**/*.py
- "l" should be a builtin using the shell's facilities to list dirs and output should be like `/bin/ls -plAhG`
- "c" should be a builtin to clear the screen

