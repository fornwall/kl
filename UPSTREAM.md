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

The render state has no hyperlink data, so passing OSC 8 through would take a
grid reference lookup for each linked cell, which the documentation of
`grid-introspection` warns against using in render loops. A hyperlink getter on
the render state's cells would fit a renderer better. Not implemented in kl yet.

### Reverse video does not mark anything dirty

`Terminal.Dirty` has a `reverse_colors` flag, documented as set when the mode is
modified, but only the Ghostty app's stream handler sets it. `setMode` in
`stream_terminal.zig`, which is what libghostty-vt runs, does nothing for the
mode. The foreground and background of the render state do trade places, but no
row is marked dirty, so a renderer leaves everything nothing wrote to in the old
colors.

Today: kl compares the colors of each frame with the last one, and draws every
row if they differ. If the library sets the flag: that comparison can go.

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
