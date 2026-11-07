# Plan

kl is a small terminal multiplexer that follows tmux, with a single session,
built on libghostty-vt. This file tracks where it stands and where it goes next.
[README.md](README.md) has the decisions behind it and its current limitations.

## Where it stands

kl has the core of tmux, with no configuration:

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
- Several clients at once, showing the same window at the size of the one active
  last, each with its own prompt, keys and mouse. One that stops reading holds up
  nobody else. `list-clients`, `detach-client` and `C-b D` take care of them.
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
| Linux x86-64     | yes    | yes, here and in CI |
| Linux aarch64    | yes    | yes, in CI |
| macOS aarch64    | yes    | in CI: the unit tests, and about 340 of 368 end-to-end tests |

CI runs formatting, clippy and all tests on each of them, and checks that
libghostty-vt really is linked statically.

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
  and cursor color changes, reverse video and double width lines, apart from the
  window's padding, since a program's colors stay in its pane.
- **Images.** Raw RGB and RGBA and PNG images sent inline, scaled, cropped, offset
  in a cell, above and below text, deleted and replaced are the same pixel for
  pixel. A placeholder image that is not the shape of its cells is one row of
  pixels off. An image cut off at each edge of a pane is the same, and so is one
  scrolling out of view and back from the history, but for the cursor of copy mode,
  and one in a window being resized. A scaled image cut off at the edge of a pane
  has colors one step off from resampling. After switching windows, a list on top
  of it and detaching and attaching, an image looks as it did before. Replies to
  programs match, but for refusing files. A raw image of 48 MB and a PNG of 6000 by
  4000 pixels show after about 1 and 2.3 seconds, against 1.7 and 1.6 directly. Next
  to them, a pane updating ten times a second missed 0.13 and 0.31 seconds at most,
  and commands were answered within 0.1 seconds. Leaving a window, detaching or
  closing the pane while such an image is on its way breaks nothing. With two
  windows on the session, both show images the same as one does, also after
  switching windows and after one detaches. One that stops reading holds nothing up
  for the other, and gets the image once it goes on. A window of another size and
  one with other cells were only looked at.
- **Pointer shapes.** Read through XFixes, 9 shapes by name, an unknown name, the
  default and a program taking the mouse and letting go of it give the cursor they
  give directly, and the shape goes with the pane under the pointer. An animated
  cursor differs from one look to the next, so that one was only compared by size.
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

Not covered: Wayland, the macOS app, a GPU, a real input method such as ibus,
`restart-server` and what is hard to see from outside, such as notifications, the
bell and following a live color scheme change.

The input parser, the protocol decoders, the command language and the way from
parsed events and program output through libghostty-vt have been fuzzed on Linux
x86-64, for 11 to 21 minutes on 4 cores each without a crash. See
[fuzz/README.md](fuzz/README.md).

Three independent reviews went through the code, one each on the server and its
clients, on the boundary with libghostty-vt and the outer terminal, and on the
parsers and logic, with random commands and comparisons with tmux 3.8 on top.
What they found that is not fixed is under known defects below. They found no way
for a program to get out of the sequence carrying its data when the outer terminal
is Ghostty, sending all 256 byte values through every path to it, and no memory
safety defect.

`KL_TMPDIR` moves the socket directory, as `TMUX_TMPDIR` does in tmux. It exists
for the tests, and is not meant as a setting.

## Next iterations

### 2. Confidence on every target

- Find out how the outer terminal's replies reach programs as typing on macOS.
  `panes_do_not_inherit_what_the_first_client_had_open` failed once with
  `$ 4;36;rgb:0000/afaf/8787echo ...`: part of a reply to the color queries kl sends
  on attaching went to the shell as if typed. All 256 palette entries are asked
  for at once, and a guess is that this overflows the small input queue of a
  terminal on macOS, so that bytes get lost and what is left of a reply is no longer
  one. This has not been confirmed. If it is so, it happens outside the tests too,
  and asking for a few entries at a time would be the fix. It may be behind some
  of the failures below.
- Get the end-to-end tests passing on macOS. What is known of the 30 or so that
  fail, which is not always the same 30:
  - Those waiting for a window called `sh`, which `/bin/sh` is not on macOS: it
    shows up as `bash`. `control_keys_reach_programs` is one, where the keys
    themselves worked. So are others that time out on `0:sh*` or `1:sh*`.
  - `a_failed_restart_leaves_the_server_running`, as there is no `/bin/false`.
  - Not looked into, so these may be real bugs: layouts and moving panes with keys
    (`spreads_panes_out_evenly`, `space_goes_through_the_layouts_like_tmux`,
    `alt_keys_pick_layouts`, `control_o_rotates`, `swaps_panes_with_their_neighbors`,
    `breaks_a_pane_out_to_its_own_window`, the tests of the mark), sizes with
    several clients (`the_window_takes_the_size_of_the_client_active_last`, and
    those on smaller clients), focus reports
    (`programs_have_the_keyboard_while_any_terminal_does`), the color scheme
    (`programs_hear_when_the_color_scheme_changes`), images, and clients that stop
    reading or come back from a restart.
- See what happens to what is on its way to a program on macOS when its pty is
  full. In the test of replies piling up, less was left waiting than on Linux. A
  failed write drops all of it, which is not what a slow reader should get.
- Try kl in the Ghostty app on macOS. Option composing a character is the first
  thing to look at: kl takes alt that reaches it for alt, as the outer terminal
  has decided what option means, but text that comes with it has not been tried.

### Known defects

Found by the reviews and left for later, most severe first. Each was reproduced,
unless it says otherwise.

- A program can freeze every pane. Replacing a large image's frame with kitty's
  `a=c` takes 13 ms in the library, and 64 KiB of those keeps a single write busy
  for 38 seconds. Feeding `vt_write` small slices with a time budget would fix
  this and anything like it. It needs the event loop to come back for the rest.
- A program flooding the clipboard from a hidden pane can keep frames from a
  client on a slow link. Frames wait for its output to be empty, and what is no
  frame keeps topping that up.
- Text can overrun the divider, where the outer terminal disagrees with a pane
  about how wide a cell is. A pane turning off grapheme clustering with
  `?2027l` gets there with emoji, and a flag written right to left shifts what
  comes after it. The renderer trusts the cell width.
- A resize in a split of a cell or less leaves it with no share of the space, so
  its panes stay at zero size until another layout. The older layout notation has
  no size check, which gets there too.
- A split keeps one cell for a child that needs more because it is a split itself,
  so panes can go to zero size in windows with plenty of room. `select-layout -o`
  then refuses the layout it wrote.
- Line selections and marks in copy mode keep absolute rows, which shift when
  history is trimmed: a program can change what gets copied by printing more.
- `restart-server`:
  - A failed `exec` is reported as success, as clients have been told to come
    back by then. The server carries on, without copy modes.
  - A failed write leaves a partial state file behind (not reproduced).
  - Focus does not survive it, so a program can miss that it got focus back.
  - From the prompt it fails after the binary was replaced, as `current_exe` says
    `(deleted)` on Linux. From the command line it works.
  - Panes hidden by a zoom list as 0x0 until unzoomed.
- `-c` formats go by the active pane rather than the target.
- Hyperlinks amplify. 45 KB from a program came to an 8.4 MB frame, which only
  delays that client.
- `?2026l` is not part of leaving the terminal, so a frame cut off by a dying
  server leaves synchronized output on until the terminal gives up (not
  reproduced).
- Two servers can end up running when many start at once after a crash left a
  stale socket, as binding checks and then removes it.
- `programs_get_real_pixels_when_they_ask` timed out once, and 120 runs under load
  did not reproduce it.

### Differences from tmux that are bugs

- `kill-pane` gives the space to the previous pane, where tmux 3.8 gives it to the
  next.
- `swap-pane -d` with an explicit source moves focus to the other pane.
- `+0` is taken for index 0, `kill-window -t 2.%1` goes by the pane and ignores the
  window, and `=kl:1` is refused.

### 3. Getting around

What is missing for daily use compared with tmux.

- A preview in the list of `C-b w`, as in tmux. It takes more than drawing in the
  list. Reading a pane takes a terminal that can change, which only `Canvas`
  gets, once for all clients and for panes on display, while which pane to preview
  is up to each client. A hidden pane has to wake up the frame too, and lists need
  to know their height for paging and clicks. `screen_line` could read one without
  any of that, but only as plain text and too slowly for drawing.
- `main-pane-width` and the like are fixed at their defaults for lack of settings.

### 4. Copy mode

What is left of tmux's `copy-mode-vi` table, and the differences that remain:

- A few commands of `send-keys -X`: `select-word`, `selection-mode`,
  `previous-matching-bracket`, `cursor-centre-horizontal`,
  `cursor-centre-vertical`, `recentre-top-bottom`, `toggle-position` (also on
  `P`), line numbers, `scroll-exit-*`, `scroll-to-mouse` and the incremental
  searches. They do nothing here, as anything else that is no command does.
- Piping what is copied to a program: `copy-pipe` with a command and `pipe`, which
  say that they are not supported. That needs a way to run programs that does not
  hold up the server.
- Copy mode shows the live screen where tmux shows a copy of it. At the bottom of
  the history, new output moves the text under the cursor.
- Searching ignores case and takes the text literally, see
  [UPSTREAM.md](UPSTREAM.md).

### 5. Passing more through

Things panes can ask for that stop at kl today.

- Playing animations, which stay on their current frame. It takes calling
  `ghostty_kitty_graphics_animation_tick` from the event loop, and each frame
  means sending all its pixels again, so measure that first.
- Images from files and shared memory (`OPT_KITTY_IMAGE_MEDIUM_*`), which are
  refused for now. Programs that try fall back to sending them inline.
- Asking the outer terminal whether it takes images at all. Programs are told yes
  either way, and pixels go out to a terminal that ignores them.
- Going easy on slow links. Pixels go out uncompressed, between frames, 72 MB took
  2.3 seconds locally, and an image that left the screen is sent again when it
  comes back.

### 6. Clients

- Moving the view of a client smaller than the window by hand, with `refresh-client
  -L` and the like. It follows the cursor for now.
- What is left of tmux's flags for clients: read-only clients with `attach-session
  -r`, and `-P`, `-E` and `-x` to end what a client runs in along with it.

### Later: settings

Deliberately left out so far. When the time comes, the question to answer first
is which defaults people actually want to change, rather than how to expose
everything tmux exposes.
