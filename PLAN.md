# Plan

kl is a small terminal multiplexer that follows tmux, with a single session,
built on libghostty-vt. This file tracks where it stands and where it goes next.

## Where it stands

The first iteration has the core of tmux, with no configuration:

- A background server that keeps panes running, with a client that attaches to
  it. Starting `kl` attaches, creating the session if needed.
- Windows and panes: creating, closing, switching, renaming, splitting, zooming,
  resizing, with the status line and tmux's default keys on the `C-b` prefix.
- Copy mode, the `:` prompt, and the same commands from the command line.
- The mouse is always on: select panes and windows, drag borders, scroll, select
  text (double and triple click too), and paste with the middle button. Programs
  that ask for the mouse get it, and holding shift takes it back.
- Full-fidelity input: the kitty keyboard protocol towards the outer terminal,
  and libghostty-vt's encoders towards panes. Programs get the encoding they
  asked for, as if no multiplexer was in between.
- Clipboard writes, bells and titles reach the outer terminal, and its default
  colors reach programs that ask for them.

### What has and has not been verified

| Target           | Builds | Tests run |
| ---------------- | ------ | --------- |
| Linux x86-64     | yes    | yes       |
| Linux aarch64    | type-checked and linted, with libghostty-vt built | no |
| macOS aarch64    | type-checked and linted, with libghostty-vt built | no |

The end-to-end tests use libghostty-vt as the outer terminal, so they exercise
the core of Ghostty and the exact key sequences it encodes. They do not cover
the Ghostty application itself, which has not been tried yet. Real programs
(nvim, less, htop) have been tried with tmux as the outer terminal, which also
covers the legacy input path.

`KL_TMPDIR` moves the socket directory, as `TMUX_TMPDIR` does in tmux. It exists
for the tests, and is not meant as a setting.

## Next iterations

### 2. Confidence on every target

Before adding much more, close the gaps in the table above.

- CI on Linux x86-64, Linux aarch64 and macOS aarch64, running clippy and all
  tests. Linting every target matters: `cfg`-gated code is only linted on its
  own platform.
- Try it for real in Ghostty, and fix what that finds.
- Fuzz the input parser and the protocol decoder.

### 3. Getting around

What is missing for daily use compared with tmux.

- `C-b ?` listing keys, `C-b w` to choose a window, `C-b q` to show pane numbers.
- `C-b {` and `}` to swap panes, `C-b !` to break a pane out, `join-pane`.
- `C-b Space` and layouts such as even-horizontal, tiled and main-vertical.
- Repeatable keys, so that `C-b C-Up C-Up` resizes twice.
- `-t` targets for commands, and format strings for `list-*` and
  `display-message`.
- Activity monitoring for windows, next to the bell flag.
- Show a window's name right away, instead of at the next tick.

### 4. Copy mode

- Search with `/`, `?`, `n` and `N`, which means adding libghostty-vt's `search`
  feature to the build.
- Word motions, jumping to characters, rectangle selection.
- Scrolling while dragging a selection past the edge of a pane, which the
  library's selection gesture API already handles.
- Jumping between shell prompts, with semantic prompts from OSC 133.
- Named paste buffers and `choose-buffer`.

### 5. Passing more through

Things panes can ask for that stop at kl today.

- OSC 8 hyperlinks. Cells keep them, but the render state does not expose them,
  so see [UPSTREAM.md](UPSTREAM.md).
- Kitty graphics, which is a project of its own: placements have to be clipped to
  panes and follow scrolling. libghostty-vt's `kitty_graphics` feature tracks
  images and placements.
- Reading the clipboard (OSC 52 `?`), which needs a decision on who may do that.
- Desktop notifications and progress reports (OSC 9, 777, 9;4).
- Mouse pointer shapes (OSC 22), and SGR-pixels mouse reports at real pixels.
- Palette changes (OSC 4). Colors are passed on by index, so a program that
  redefines the palette looks different from running it directly.
- Live color scheme changes (mode 2031), which today are only derived from the
  background color reported at attach.
- Reverse video (DECSCNM).

### 6. Clients and persistence

- Several clients at once. A new client currently takes over from the old one.
  This raises the question of which size wins, and tmux's answers are a good
  start.
- Refreshing `SSH_AUTH_SOCK`, `DISPLAY` and the like on attach.
- Surviving a server restart, with libghostty-vt's `snapshot` feature.
- Compressing idle scrollback with `ghostty_terminal_compress`.
- `TERM=xterm-ghostty` where Ghostty started the server, `xterm-256color`
  otherwise. Installing the terminfo entry on demand would give every host the
  accurate one, which matters most over ssh.

### Later: settings

Deliberately left out so far. When the time comes, the question to answer first
is which defaults people actually want to change, rather than how to expose
everything tmux exposes.
