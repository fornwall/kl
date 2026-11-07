# kl

Klethron Terminal Multiplexer: a small terminal multiplexer that follows tmux,
with a single session, built on
[libghostty-vt](https://github.com/ghostty-org/ghostty).

## Using it

`kl` attaches to the session, starting it if needed. Keys and commands follow
tmux, with `C-b` as the prefix, and `kl --help` lists what exists. The same
commands work from the command line, from key bindings and from the `:` prompt.

## Building it

```sh
cargo build --release
```

This needs Rust, git and Zig 0.17. The first build fetches libghostty-vt at the
commit pinned in `build.rs` and builds it, which takes a few minutes.

## Decisions

These are choices rather than gaps, and each has a reason.

- **One session.** Windows and panes are all there is to organize. tmux's
  sessions are left out on purpose, so commands only ever target windows and
  panes.
- **Linux on x86-64 and aarch64, and macOS on aarch64.** Nothing else is a target.
- **A modern terminal is assumed,** which for now means the latest Ghostty
  release. Other terminals may work, but kl does not hold back for them.
- **libghostty-vt is statically linked,** from a pinned commit on ghostty's `main`
  branch, never a release, branch or fork. [UPSTREAM.md](UPSTREAM.md) lists what
  kl works around, including the patch it applies.
- **No configuration yet.** The question to answer first is which defaults people
  actually want to change, rather than how to expose everything tmux exposes.
- **The mouse is always on.** Programs that ask for it get it, and holding shift
  takes it back.
- **Panes always get `TERM=xterm-ghostty`,** since programs talk to libghostty-vt
  whatever the outer terminal is. kl carries the terminfo entry and installs it
  next to its socket, so it works on hosts without Ghostty. As with Ghostty
  itself, a host reached with `ssh` from a pane needs the entry too, or `TERM` set
  to something it knows.
- **Upgrades keep programs running, crashes and reboots do not.** `kl
  restart-server` turns the running server into a new binary by way of `exec`.
  Nothing can keep programs alive through a crash or a reboot, so those end the
  session. Bringing back the layout and what panes showed would mean regularly
  writing what is on screen to disk, and that is left out for now.

## Limitations

What does not work today. [PLAN.md](PLAN.md) has what is planned, and how far
each target has been verified.

- **One client at a time.** A new client takes over from the one attached.
- **Images have some limits.** Animations stay on their current frame, images
  have to be sent inline rather than from a file, and pixels go out uncompressed,
  so they are slow over a slow link. An image scaled to a size that is not a whole
  number of cells can come out up to half a cell off.
- **Hyperlinks only go by their URI.** libghostty-vt does not give out their
  `id`, so links in a pane with the same URI highlight together, even if the
  program kept them apart. A URI with anything but printable ASCII in it arrives
  percent-encoded, and one that this or kl's own `id` takes past about 2 KB loses
  its link in Ghostty.
- **The terminal's title does not come back in Ghostty.** It has no title stack,
  so leaving kl clears the title rather than restoring it, until something like
  the shell's prompt sets it again.
- **Copy mode differs from tmux in a few ways.** It shows the live screen rather
  than a copy of it, so at the bottom of the history new output moves the text
  under the cursor. Searching ignores case and takes the text literally, where
  tmux takes a regular expression. Relative paths for `save-buffer` and
  `load-buffer` go by the active pane's directory, not the client's.
- **Panes are numbered by where they are.** tmux keeps its own order, which
  matters in the mirrored layouts: the main pane is number 0 there, and the last
  one here. Which pane is the main one still carries over between layouts.
- **Formats cover what people actually use.** That is variables, conditions,
  comparisons and common modifiers. `s/from/to/` replaces plain text rather than
  a regular expression, and there is no `#()`, `#[]` or looping over windows.
- **Programs cannot read the clipboard.** They can write to it, but who should be
  allowed to read it is undecided, so OSC 52 queries get no answer.
- **`restart-server` has some rough edges:**
  - A key typed at the moment of the restart can be lost, along with input a
    program had not read yet.
  - Copy mode ends, and so do open prompts.
  - Images are gone until their program sends them again.
  - A pane in the middle of an escape sequence longer than 64 KiB comes back
    blank. Its program keeps running.
  - The binary has to be able to read the running server's state, and is refused
    otherwise, which leaves the server as it was.
  - What panes hold passes through a file next to the socket for a moment, in a
    directory only the user can enter.
- **Tests have only run on Linux x86-64.** Linux aarch64 and macOS have been
  type-checked and linted, but never run. The CI that would cover them has not
  run yet either, since this repository has no remote.
