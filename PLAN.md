# Plan

kl is a small terminal multiplexer that follows tmux, with a single session,
built on libghostty-vt. This file tracks where it stands and where it goes next.
[README.md](README.md) has the decisions behind it and its current limitations.

## Where it stands

The first iteration has the core of tmux, with no configuration:

- A background server that keeps panes running, with a client that attaches to
  it. Starting `kl` attaches, creating the session if needed.
- Windows and panes: creating, closing, switching, renaming, splitting, zooming,
  resizing, with the status line and tmux's default keys on the `C-b` prefix.
- Copy mode, the `:` prompt, and the same commands from the command line.
- Upgrading without losing anything: `restart-server` turns the server into a new
  binary, with programs still running and clients attached again.
- The mouse is always on: select panes and windows, drag borders, scroll, select
  text (double and triple click too), and paste with the middle button. Programs
  that ask for the mouse get it, and holding shift takes it back.
- Full-fidelity input: the kitty keyboard protocol towards the outer terminal,
  and libghostty-vt's encoders towards panes. Programs get the encoding they
  asked for, as if no multiplexer was in between.
- Clipboard writes, bells, titles, hyperlinks, desktop notifications and progress
  reports reach the outer terminal, as does the pointer shape of the pane under
  the pointer. Its colors and color scheme reach programs that ask for them, also
  when they change. Colors a program changes for itself, with OSC 4, 10 and 11 or
  reverse video, stay in its pane.

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

The input parser, the protocol decoders, the command language and the way from
parsed events and program output through libghostty-vt have been fuzzed on Linux
x86-64, for 5 to 11 minutes on 4 cores each without a crash. See
[fuzz/README.md](fuzz/README.md).

`KL_TMPDIR` moves the socket directory, as `TMUX_TMPDIR` does in tmux. It exists
for the tests, and is not meant as a setting.

## Next iterations

### 2. Confidence on every target

Before adding much more, close the gaps in the table above.

- Get CI running. `.github/workflows/ci.yml` covers Linux x86-64, Linux aarch64
  and macOS aarch64 with clippy and all tests, but this repository has no remote
  yet, so it has never run. The first run is what fills in the table above.
- Try it for real in Ghostty, and fix what that finds.

### 3. Getting around

What is missing for daily use compared with tmux.

- A preview in the list of `C-b w`, and going down to panes there, as in tmux.
- `C-b {` and `}` to swap panes, `C-b !` to break a pane out, `join-pane`.
- `C-b Space` and layouts such as even-horizontal, tiled and main-vertical.
- `-t` targets for commands, and format strings for `list-*` and
  `display-message`.

### 4. Copy mode

- Jumping between shell prompts, with semantic prompts from OSC 133.
- Named paste buffers and `choose-buffer`.

### 5. Passing more through

Things panes can ask for that stop at kl today.

- Kitty graphics, which is a project of its own: placements have to be clipped to
  panes and follow scrolling. libghostty-vt's `kitty_graphics` feature tracks
  images and placements.
- Reading the clipboard (OSC 52 `?`), which needs a decision on who may do that.

### 6. Clients and persistence

- Several clients at once. A new client currently takes over from the old one.
  This raises the question of which size wins, and tmux's answers are a good
  start.

### Later: settings

Deliberately left out so far. When the time comes, the question to answer first
is which defaults people actually want to change, rather than how to expose
everything tmux exposes.
