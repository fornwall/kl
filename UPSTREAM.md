# Upstream

Changes in the components kl builds on that would help this project. Each entry
says what kl does today, so that the workaround can go when the change lands.

## libghostty-vt

### Split sections in the static library

[ghostty#13708](https://github.com/ghostty-org/ghostty/pull/13708) puts each
function and data item of the static library in its own section, so that
`--gc-sections` can drop what a program does not use. It was closed unmerged
after a request for more benchmark data.

kl applies its net diff on top of the pin, in
`patches/ghostty-split-static-sections.patch`. Measured on kl's stripped release
binary on Linux x86-64, built from the same commit with the same features:

| libghostty-vt | Text sections in the Zig unit | `libghostty-vt.a` | `kl`        |
| ------------- | ----------------------------- | ----------------- | ----------- |
| Unpatched     | 2                             | 4,572,350 B       | 2,146,392 B |
| Patched       | 976                           | 4,952,144 B       | 1,886,792 B |

That is 12.1% off the binary, for an archive 8.3% larger. It also makes enabling
an optional feature cheap, since the parts kl does not call get dropped.

When something equivalent lands on `main`: delete the patch and `apply_patch` in
`build.rs`.

### Decoding cells with only the render state

`ghostty_cell_get` only decodes a raw cell value, and `render.h` recommends it
for render state cells, but it is gated behind `grid-introspection`. kl needs it
for cell width, and for the palette index of erased backgrounds, which
`GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR` resolves to RGB.

Today: kl enables the feature, which costs about 370 bytes and nothing at
runtime. If the gate also accepts `render-state`: drop `+grid-introspection`
from `VT_FEATURES` in `build.rs`.

### Hyperlinks in the render state

The render state knows which cells are linked, since
`GHOSTTY_CELL_DATA_HAS_HYPERLINK` reads the flag from its raw cells, but not what
they link to. Only a grid reference has that, and for anything that has to send
the link along with each linked cell it paints, that falls short:

- **The right link for the cell being painted.** A grid reference reads the live
  terminal, not the snapshot, and the render state stays behind on purpose during
  a render hold. Read while drawing, it pairs old text with a new URI: without
  the workaround below, kl repainted `aaa` from before a hold with
  `https://new.example` from after its start. `linkCells` documents the same
  catch. Threaded embedders also need terminal access to draw links, which the
  render state is there to avoid.
- **No `id`.** `ghostty_grid_ref_hyperlink_uri` returns only the URI, but links
  match on `id` plus URI (`src/terminal/hyperlink.zig`). Opening a link without
  an `id` gets a fresh implicit one, so if a partial repaint did that, the link
  would be split into pieces that highlight separately. The only way to get at an
  `id` from C is the text of the VT formatter, and only if it is explicit.

Speed is not the case to make, whatever `ghostty_terminal_grid_ref`, the
documentation of `grid-introspection` and the grid guide say about render loops:
"use grid references" would be a fair reply. Timed in C against kl's build of the
library, with the median of 301 frames reading every row, the lookups for every
linked cell add about 150 µs to the 125 µs it takes to read a 200×50 render state
with 9,000 linked cells, and 870 µs to 480 µs at 400×100 with all 40,000 linked.
That is 1% and 5% of a 60 Hz frame, and kl normally reads only dirty rows.

Today: kl copies URIs with grid references right after each render state update,
while the two still agree, and that includes the update where a hold starts,
which runs in the middle of a write. Only rows and cells with the hyperlink flag
pay for this. Cells refer to a number for each URI and pane, sent as `id=kl<hex>`
the way tmux's `hyperlinks.c` sends `tmux%llX`, so two links in a pane with the
same URI and different ids are one link to kl. When the render state has links:
delete `src/ghostty/links.rs` and `Effects::copy_links` in `src/ghostty/mod.rs`.

A shape that should work:

- A list of link runs per row, in the row arena like `Row.placeholders`, since a
  new field on `Cell` would grow every cell.
- Built only for rows with the hyperlink flag, with one URI copy per run,
  exposing the URI and the `id`. Not the internal `hyperlink.Id`, which only means
  something within one page.
- URI bytes live in page memory, so the copy has to happen in `beginUpdate` with
  the lock held. That path is heavily tuned and the Ghostty app would not read
  the result, so it could be opt-in like `overscan_request`. Comments in
  `render.zig` already count hyperlinks among the managed memory it copies.

This is mostly about multiplexers, and the reason is who handles the pointer. A
GUI sees the hover itself, so it needs one cell's URI at that moment, which is
what a grid reference is good for. A multiplexer never sees it, as the outer
terminal does, so it has to send the URI with every linked cell. A terminal
widget inside a TUI is in the same position. GUI embedders have a smaller gap:
`linkCells` is Zig only, so from C they cannot reproduce whole-link highlighting
on hover, which link runs would fix as well.

Next step: a discussion in Feature Requests rather than a pull request, with kl
as a working example. Nothing has been posted yet.

### Reverse video does not mark anything dirty

`Terminal.Dirty` has a `reverse_colors` flag, documented as set when the mode is
modified, but only the Ghostty app's stream handler sets it. `setMode` in
`stream_terminal.zig`, which is what libghostty-vt runs, does nothing for the
mode. The foreground and background of the render state do trade places, but no
row is marked dirty, so a renderer leaves everything nothing wrote to in the old
colors.

Today: kl compares the colors of each frame with the last one, and draws every
row if they differ. If the library sets the flag: that comparison can go.

### Searching like tmux

`GHOSTTY_SEARCH_OPT_NEEDLE` always ignores the case of ASCII letters, and takes
the text literally. tmux only ignores case for text that is all lower case, and
`/` in its copy mode takes a regular expression.

Today: kl searches the way the library does. An option for case-sensitive
matching would give it tmux's behavior for the case, and regular expressions
would need a matcher in the library, since kl only sees the matches.

### Source tarballs for a commit

`libghostty-vt-source.tar.gz` holds just what the library needs, but it is only
published for tagged releases and as a moving `tip`. Pinning a commit on `main`
therefore means fetching the whole repository, which is a 143 MB checkout. A
source tarball addressable by commit would cut that down and drop the need for
`git` in the build. The same goes for the dependency tarball, which would remove
the package fetching step below.

### A compiled terminfo entry

kl gives panes `TERM=xterm-ghostty`, with the compiled entry in
`terminfo/xterm-ghostty` installed next to its socket, since few hosts have it.
The entry is the source of truth in `src/terminfo/ghostty.zig`, but only the
build of the Ghostty app turns that into a file, so kl keeps a copy from an
installed Ghostty. `ghostty_terminfo()` or an install step of `-Demit-lib-vt`
would keep it in step with the pin, and let the file go.

### Scrollback default

`GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES` defaults to 10,000 bytes, about one
page. It is documented, but easy to miss: a program using the defaults gets
next to no history without any sign of why. A default nearer to the 10 MB of the
Ghostty app would be friendlier. Today: kl sets 10 MB itself.

## Ghostty

Found by running kl in the app, version 1.4.0.

### The title stack

`CSI 22 t` and `CSI 23 t` are parsed, but ignored in `stream_handler.zig`, and
reporting the title is off by default. That leaves a program no way to put back
the title it found.

Today: kl pushes and pops the title for the terminals that can, and sets an empty
title when leaving, which Ghostty takes as going back to no title. What was there
before kl is lost, until something like the shell's prompt sets it again.

### Replies to long color queries

The reply to an OSC 4 query is built in a 1 KiB buffer. A sequence asking for more
than a few dozen colors logs `error.WriteFailed`, and no reply is sent at all.

Today: kl asks for each palette entry in a sequence of its own.

## Zig

### Fetching dependencies of dependencies

`zig build --fetch` does not fetch the dependencies of dependencies
([zig#20976](https://github.com/ziglang/zig/issues/20976)), and `zig fetch` only
saves archives, which `zig build --system` cannot read. Ghostty works around this
with `nix/build-support/fetch-zig-cache.sh`, which kl runs. A plain `zig build`
is no alternative, since it fetches what the Ghostty app needs too: 337 MB of
packages, against 87 MB for libghostty-vt.

### Build crash with archives in the cache

Ghostty's packaging guide says that with Zig 0.17, an archive of a package in the
global cache can crash a `--system` build with "reached unreachable code", so
that the build needs an empty cache. kl has not reproduced this, but follows the
guidance in a way that keeps the rest of the cache: it moves the fetched packages
out of the cache directory instead of starting over, which saves compiling the
build runner a second time (78 s down to 2 s to get to the same point in the
build).

When Zig fixes these two: `fetch_packages` in `build.rs` can go.
