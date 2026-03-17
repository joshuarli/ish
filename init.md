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

