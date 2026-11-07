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
  kl works around, including the two patches it applies.
- **No configuration yet.** The question to answer first is which defaults people
  actually want to change, rather than how to expose everything tmux exposes.
- **The mouse is always on.** Programs that ask for it get it. Holding shift
  leaves it to Ghostty, as with tmux: its own selection, which knows nothing of
  panes, and ctrl+shift+click for links, which it opens only while it has the
  mouse.
- **Panes always get `TERM=xterm-ghostty`,** since programs talk to libghostty-vt
  whatever the outer terminal is. kl carries the terminfo entry and installs it
  next to its socket, so it works on hosts without Ghostty. As with Ghostty
  itself, a host reached with `ssh` from a pane needs the entry too, or `TERM` set
  to something it knows.
- **Several clients show the same window,** as there is only one session. It
  takes the size of the client active last, as with tmux's `window-size latest`.
  Keys, the mouse being pressed, pasting and resizing count as activity, as does
  a terminal getting the keyboard, but the pointer just moving does not: kl hears
  of that all the time, where tmux does not. A terminal of another size shows the
  window as in tmux: a smaller one follows the cursor and says where it is, and a
  larger one has dots around it.
- **What belongs to one person stays with their client.** That is the prefix,
  the prompt, messages, lists, mouse drags and the pointer shape, which goes by the
  pane under that client's pointer. Copy mode stays with the pane, as in tmux.
- **There is no telling who a program means,** so clipboard writes, bells,
  notifications, titles and progress reach every client, and so does what gets
  copied. tmux does the same with what it passes on, and seeing a notification
  twice beats losing it.
- **Programs get one answer about colors,** and about the color scheme and the
  default cursor: that of the client active last, which is the one the window fits.
  tmux goes by the first client that knows. When terminals disagree, programs that
  asked to hear of changes get told as the window moves between them.
- **A program has the keyboard while any terminal showing it does.**
- **Programs cannot read the clipboard.** They can write to it, but an OSC 52 read
  gets no answer, the same as with tmux by default and with herdr. An answer goes
  to a program's pty, which can lead to another host, so anything there could
  quietly read what was copied here.
- **Notifications get through from every pane,** seen or not, since telling you
  about something out of sight is what they are for.
- **Images are worth their size.** Kitty graphics and PNG decoding add about
  390 KB to the binary. PNG comes from a one line patch that turns on Ghostty's
  own decoder, rather than a second decoder of untrusted input in the server.
- **Copy mode adds to tmux's keys.** `y` copies, and `[` and `]` jump between
  prompts. tmux leaves `y` unbound, and has that jump on other keys only.
- **Upgrades keep programs running, crashes and reboots do not.** `kl
  restart-server` turns the running server into a new binary by way of `exec`.
  Nothing can keep programs alive through a crash or a reboot, so those end the
  session. Bringing back the layout and what panes showed would mean regularly
  writing what is on screen to disk, and that is left out for now.

## Limitations

What does not work today. [PLAN.md](PLAN.md) has what is planned, and how far
each target has been verified.

- **A client smaller than the window only follows the cursor.** tmux can also
  move such a view around by hand, with `refresh-client -L` and the like.
- **Images have some limits.** Animations stay on their current frame, images
  have to be sent inline rather than from a file, and pixels go out uncompressed,
  so they are slow over a slow link. An image stretched out of proportion, or
  scaled to part of a cell in both directions, can come out up to half a cell off.
  A large PNG holds up every pane while it is decoded, which took 0.3 seconds for
  6000 by 4000 pixels.
- **A program can slow everything down.** The server has one thread, and it can
  not stop halfway through what a program wrote. Some image commands are slow
  enough for a program sending them on purpose to hold up every pane for as long
  as it keeps going, and one flooding the clipboard can keep frames from reaching a
  client on a slow link. Nothing is lost, and a client shows at most 2000 images at
  a time. Replies past 96 MiB to a program that does not read them are dropped.
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
- **Layouts come out as the JSON of tmux 3.8.** `select-layout` takes that and the
  older notation with a checksum, but `#{window_layout}` only writes the former,
  like tmux outside of control mode.
- **A layout written down keeps its proportions in a window of another size.**
  tmux adds or takes the difference evenly from each pane, which can leave some
  of them a single cell wide, as it does when a window changes size.
- **Panes are numbered by where they are.** tmux keeps its own order, which
  matters in the mirrored layouts: the main pane is number 0 there, and the last
  one here. Which pane is the main one still carries over between layouts.
- **Formats cover what people actually use.** That is variables, conditions,
  comparisons and common modifiers. `s/from/to/` replaces plain text rather than
  a regular expression, and there is no `#()`, `#[]` or looping over windows.
- **`find-window` has no regular expressions.** There is no `-r`, and its
  patterns know `*` and `?`, but not the `[...]` of `glob(7)`.
- **`restart-server` has some rough edges:**
  - A key typed at the moment of the restart can be lost, along with input a
    program had not read yet.
  - Copy mode ends, and so do open prompts.
  - Images are gone until their program sends them again.
  - With clients of different sizes, the window can come back with the size of
    another one than it had, until someone types.
  - A pane in the middle of an escape sequence longer than 64 KiB comes back
    blank. Its program keeps running.
  - The binary has to be able to read the running server's state, and is refused
    otherwise, which leaves the server as it was.
  - What panes hold passes through a file next to the socket for a moment, in a
    directory only the user can enter.
- **macOS is not all there yet.** kl builds and runs there, and CI runs all tests
  on every target. They pass on Linux x86-64 and aarch64. On macOS the unit tests
  pass and about 30 of 368 end-to-end tests fail, and [PLAN.md](PLAN.md) says what
  is known of them. Some may be real bugs: at least once, part of the outer
  terminal's reply to a color query was typed into a shell on attaching. The
  Ghostty app has only been tried on Linux, so nothing says yet what option
  composing a character does on macOS.
