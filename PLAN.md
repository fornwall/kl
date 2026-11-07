# Plan

kl is a small terminal multiplexer that follows tmux, with a single session,
built on libghostty-vt. This file tracks where it stands and where it goes next.
[README.md](README.md) has the decisions behind it and its current limitations.

## Where it stands

The first iteration has the core of tmux, with no configuration:

- A background server that keeps panes running, with a client that attaches to
  it. Starting `kl` attaches, creating the session if needed.
- Windows and panes: creating, closing, switching, renaming, splitting, zooming,
  resizing, swapping, moving between windows and tmux's layouts, with the status
  line and tmux's default keys on the `C-b` prefix.
- tmux's `-t` targets and formats, for scripts as much as for people.
- Copy mode with tmux's vi keys, search and paste buffers, the `:` prompt, and
  the same commands from the command line.
- Upgrading without losing anything: `restart-server` turns the server into a new
  binary, with programs still running and clients attached again.
- The mouse is always on: select panes and windows, drag borders, scroll, select
  text (double and triple click too), and paste with the middle button. Programs
  that ask for the mouse get it, and holding shift leaves it to Ghostty.
- Full-fidelity input: the kitty keyboard protocol towards the outer terminal,
  and libghostty-vt's encoders towards panes. Programs get the encoding they
  asked for, as if no multiplexer was in between.
- Clipboard writes, bells, titles, hyperlinks, desktop notifications and progress
  reports reach the outer terminal, as does the pointer shape of the pane under
  the pointer. Its colors and color scheme reach programs that ask for them, also
  when they change. Colors a program changes for itself, with OSC 4, 10 and 11 or
  reverse video, stay in its pane.
- Images with the kitty graphics protocol, PNG included, clipped to their pane and
  following the text as it scrolls.

### What has and has not been verified

| Target           | Builds | Tests run |
| ---------------- | ------ | --------- |
| Linux x86-64     | yes    | yes       |
| Linux aarch64    | type-checked and linted, with libghostty-vt built | no |
| macOS aarch64    | type-checked and linted, with libghostty-vt built | no |

The end-to-end tests use libghostty-vt as the outer terminal, so they exercise
the core of Ghostty and the exact key sequences its encoder produces. That leaves
out what the app adds around its core: how it fills in key events, what it keeps
for itself, and the limits of its replies. So kl has been tried in the app too.

That was Ghostty 1.4.0, the GTK build from the snap, on Linux x86-64, under Xvfb
with Mesa's software OpenGL, with keys and mouse events injected through XTEST.
Wherever it could be done, the same thing was done with a program directly in the
app and in a pane of the same size, comparing what the program logged or what the
framebuffer held:

- **Keys.** 53 keys and combinations under legacy encoding, kitty flags 1, 3 and
  31, on the alternate screen, in application cursor and keypad mode, and with
  modifyOtherKeys 1 and 2. Programs get the same bytes, but for an added `::43` on
  the release of the numpad plus. The same goes for 11 keys on a Russian and a
  Dvorak layout, and for dead keys, on GTK's built-in input method. Bindings work,
  including `%` and `"` with shift pressed between the prefix and the key, and
  they go by physical key on the Russian layout.
- **The mouse.** 17 events in tracking modes 1000, 1002 and 1003, and in the X10,
  SGR, URXVT and SGR-pixels formats, give programs the same reports, but for how
  many motion events a drag comes to. By hand: clicking panes and windows, dragging
  borders, the wheel in history and on the alternate screen, selecting by dragging
  and by double and triple clicks, reaching the clipboard over OSC 52 with
  Ghostty's default configuration, pasting with the middle button and with
  ctrl+shift+v, shift for Ghostty's own selection, and ctrl+shift to hover a link.
- **Rendering.** A page of text attributes, underline styles and colors, 256 and
  true colors, wide characters, emoji sequences, keycaps, combining marks, right
  to left text and box drawing is the same pixel for pixel, apart from the mouse
  pointer, in a full window and in half of a split. So are palette, default color
  and cursor color changes, reverse video, kitty images and double width lines,
  apart from the window's padding, since a program's colors stay in its pane.
- **Programs.** nvim with true colors, splits and the mouse is the same pixel for
  pixel at each of 11 steps, including the bar cursor in insert mode, and across
  resizing the window. htop and less were only looked at. 3 million lines through
  a pane took 1.3 seconds, against 1.1 directly.
- **What programs are told.** 71 values, from all modes to device attributes,
  DECRQSS, sizes, colors and the color scheme, match the app. The exceptions are
  XTVERSION, which says kl, and tertiary device attributes, which the app does not
  answer.
- **Leaving.** After detaching, the last shell exiting, `kill-server`, killing the
  server and terminating the client, 31 modes, the kitty flags, the cursor style
  and the colors are back as they were. The title is not, as the README says.
- **Also tried.** A configured `cursor-style`, resizing all the way down to 18x5
  and rapidly, closing the window and attaching from a new one, synchronized
  output, focus reports, key repeat and copy mode from the keyboard.

Not covered: Wayland, the macOS app, a GPU, a real input method such as ibus, and
what is hard to see from outside, such as notifications, the bell and following a
live color scheme change.

The input parser, the protocol decoders, the command language and the way from
parsed events and program output through libghostty-vt have been fuzzed on Linux
x86-64, for 11 to 21 minutes on 4 cores each without a crash. See
[fuzz/README.md](fuzz/README.md).

`KL_TMPDIR` moves the socket directory, as `TMUX_TMPDIR` does in tmux. It exists
for the tests, and is not meant as a setting.

## Next iterations

### 2. Confidence on every target

Before adding much more, close the gaps in the table above.

- Get CI running. `.github/workflows/ci.yml` covers Linux x86-64, Linux aarch64
  and macOS aarch64 with clippy and all tests, but this repository has no remote
  yet, so it has never run. The first run is what fills in the table above.

### 3. Getting around

What is missing for daily use compared with tmux.

- A preview in the list of `C-b w`, and going down to panes there, as in tmux.
- Moving windows around: `move-window`, `swap-window`, `new-window -a`, and
  `C-b .`. Finding them: `C-b '` and `find-window` on `C-b f`.
- `resize-pane -x` and `-y`.
- `select-layout -o` and layouts in tmux's own notation, including
  `#{window_layout}`. `main-pane-width` and the like are fixed at their defaults
  for lack of settings.

### 4. Copy mode

What is left of tmux's `copy-mode-vi` table, and the differences that remain:

- Marks, on `X` and `M-x`.
- `send-keys -X`, for scripts to run the commands of copy mode.
- `-F` for `list-buffers`, and `-o` for going to the output of a command rather
  than its prompt.
- Copy mode shows the live screen where tmux shows a copy of it. At the bottom of
  the history, new output moves the text under the cursor.
- Searching ignores case and takes the text literally, see
  [UPSTREAM.md](UPSTREAM.md).

### 5. Passing more through

Things panes can ask for that stop at kl today.

- Reading the clipboard (OSC 52 `?`), which needs a decision on who may do that.
- Trying images in the Ghostty app. Everything so far ran against libghostty-vt
  as the outer terminal, and palette queries have shown that the two can differ.
- Playing animations, which stay on their current frame. It takes calling
  `ghostty_kitty_graphics_animation_tick` from the event loop, and each frame
  means sending all its pixels again, so measure that first.
- Images from files and shared memory (`OPT_KITTY_IMAGE_MEDIUM_*`), which are
  refused for now. Programs that try fall back to sending them inline.
- Asking the outer terminal whether it takes images at all. Programs are told yes
  either way, and pixels go out to a terminal that ignores them.
- Going easy on slow links. Pixels go out uncompressed, 72 MB took 2.3 seconds
  locally, and an image that left the screen is sent again when it comes back.

### 6. Clients and persistence

- Several clients at once. A new client currently takes over from the old one.
  This raises the question of which size wins, and tmux's answers are a good
  start.

### Later: settings

Deliberately left out so far. When the time comes, the question to answer first
is which defaults people actually want to change, rather than how to expose
everything tmux exposes.
