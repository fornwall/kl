//! Shows the images of panes. Programs send them to the terminal of their pane,
//! which works out what is to be seen and where, also as the text scrolls. This
//! gives the outer terminal a copy of the pixels, under an id of kl's own since
//! panes know nothing of each other, and places each image clipped to its pane.

use std::collections::{BTreeMap, VecDeque};

use super::layout::{PaneId, Rect};
use super::render::push_move;
use super::session::Session;
use crate::base64;
use crate::ghostty::{ImagePixels, ImagePlacement, ImageRect};

/// How many images a client gets shown at once. A program can place any number,
/// and comparing them on each frame takes time for the square of that.
const MAX_PLACEMENTS: usize = 2000;

/// Pixels per escape sequence, making the 4096 bytes of base64 the protocol
/// allows.
const CHUNK: usize = 3072;

/// Pixels to send with one frame. A large image takes the outer terminal seconds
/// to take in, which would otherwise freeze every pane for as long.
const FRAME_PIXELS: usize = 128 * CHUNK;

/// An image as it stands. New pixels make it another image, which is simpler than
/// updating one and just as much to send.
type ImageKey = (PaneId, u32, u64);

/// Where to show part of an image, in terms of the outer terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Placement {
    x: u16,
    y: u16,
    /// Where it starts within that cell, in pixels.
    offset: (u32, u32),
    source: ImageRect,
    /// The columns and rows to scale it to. No rows or no columns means going by
    /// the proportions of the image, and neither means its own size.
    cells: (u32, u32),
    z: i32,
}

#[derive(Clone, Copy)]
struct Shown {
    image: ImageKey,
    placement: Placement,
    id: u32,
}

/// Pixels on their way to the outer terminal.
struct Transfer {
    id: u32,
    format: u8,
    width: u32,
    height: u32,
    data: Vec<u8>,
    /// How much of `data` went out so far.
    sent: usize,
}

impl Transfer {
    fn new(id: u32, pixels: &ImagePixels<'_>) -> Self {
        Self {
            id,
            format: pixels.format,
            width: pixels.width,
            height: pixels.height,
            data: pixels.data.to_vec(),
            sent: 0,
        }
    }

    const fn finished(&self) -> bool {
        self.sent >= self.data.len()
    }

    /// Sends up to `limit` more bytes of pixels, returning how many that was.
    fn send(&mut self, limit: usize, out: &mut Vec<u8>) -> usize {
        let end = self.data.len().min(self.sent + limit);
        let part = self.data.get(self.sent..end).unwrap_or(&[]);
        let used = part.len();
        for chunk in part.chunks(CHUNK) {
            let keys = if self.sent == 0 {
                let (format, width, height) = (self.format, self.width, self.height);
                format!("a=t,f={format},s={width},v={height},i={},q=2", self.id)
            } else {
                "q=2".to_owned()
            };
            self.sent += chunk.len();
            let more = u8::from(!self.finished());
            let data = base64::encode(chunk);
            push_command(out, &format!("{keys},m={more};{data}"));
        }
        used
    }
}

/// What the outer terminal holds and shows.
#[derive(Default)]
pub struct Images {
    /// Only what is in view, so that many panes full of images do not add up.
    /// Coming back to one costs sending it again.
    sent: BTreeMap<ImageKey, u32>,
    shown: Vec<Shown>,
    /// What is still on its way, one image at a time: the terminal takes every
    /// transfer for the rest of the one it has started on.
    transfers: VecDeque<Transfer>,
    last_id: u32,
}

impl Images {
    /// Takes everything off the screen while keeping the pixels, for when the
    /// outer terminal may have moved things itself, such as in resizing. What is
    /// wanted gets placed anew.
    pub fn forget_placements(&mut self, out: &mut Vec<u8>) {
        if !self.shown.is_empty() {
            self.shown.clear();
            out.extend_from_slice(b"\x1b_Ga=d,d=a,q=2\x1b\\");
        }
    }

    const fn next_id(&mut self) -> u32 {
        self.last_id = self.last_id.wrapping_add(1);
        if self.last_id == 0 {
            self.last_id = 1;
        }
        self.last_id
    }

    /// Whether there are pixels left to send, which takes more frames.
    pub fn transferring(&self) -> bool {
        !self.transfers.is_empty()
    }

    /// Brings the outer terminal up to date. Pixels go in `transfers`, a part for
    /// each frame, and the rest in `out`. An image gets placed once all of it is
    /// there. Images stay out of sight while `covered`, as something is being shown
    /// on top of the panes, but the outer terminal gets to keep them.
    pub fn update(
        &mut self,
        session: &Session,
        (view, cell_size): (Rect, (u16, u16)),
        covered: bool,
        transfers: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) {
        let mut wanted = wanted(session, view, cell_size);
        wanted.truncate(MAX_PLACEMENTS);
        let Self {
            sent,
            transfers: queue,
            ..
        } = self;
        sent.retain(|image, id| {
            // The terminal is waiting for the rest of this one by now, and
            // deleting it can wait until it got that.
            let started = queue.front().is_some_and(|t| t.id == *id && t.sent > 0);
            let keep = started || wanted.iter().any(|(wanted, _)| wanted == image);
            if !keep {
                queue.retain(|transfer| transfer.id != *id);
                push_command(out, &format!("a=d,d=I,i={id},q=2"));
            }
            keep
        });
        if covered {
            wanted.clear();
        }
        let is_wanted = |shown: &Shown| wanted.contains(&(shown.image, shown.placement));
        let (kept, gone): (Vec<Shown>, Vec<Shown>) = self.shown.iter().partition(|s| is_wanted(s));
        for shown in gone {
            // The image itself may be gone by now, in which case this is too.
            if let Some(image) = self.sent.get(&shown.image) {
                push_command(out, &format!("a=d,d=i,i={image},p={},q=2", shown.id));
            }
        }
        self.shown = kept;
        for (image, _) in &wanted {
            self.queue_pixels(session, *image);
        }
        self.send_pixels(transfers);
        for (image, placement) in wanted {
            let known = |shown: &Shown| shown.image == image && shown.placement == placement;
            if self.shown.iter().any(known) {
                continue;
            }
            let Some(outer) = self.sent.get(&image).copied() else {
                continue;
            };
            if self.transfers.iter().any(|transfer| transfer.id == outer) {
                continue;
            }
            let id = self.next_id();
            push_move(out, placement.x, placement.y);
            push_placement(out, outer, id, &placement);
            self.shown.push(Shown {
                image,
                placement,
                id,
            });
        }
    }

    /// Gives an image an id in the outer terminal, and sets off its pixels.
    fn queue_pixels(&mut self, session: &Session, image: ImageKey) {
        if self.sent.contains_key(&image) {
            return;
        }
        let (pane, number, _) = image;
        let Some(pane) = session.panes.get(&pane) else {
            return;
        };
        // Gone if a program is holding the screen and deleted it since.
        let Some(pixels) = pane.terminal.image_pixels(number) else {
            return;
        };
        let id = self.next_id();
        self.transfers.push_back(Transfer::new(id, &pixels));
        self.sent.insert(image, id);
    }

    fn send_pixels(&mut self, out: &mut Vec<u8>) {
        let mut limit = FRAME_PIXELS;
        while limit > 0
            && let Some(transfer) = self.transfers.front_mut()
        {
            limit -= transfer.send(limit, out);
            if transfer.finished() {
                self.transfers.pop_front();
            }
        }
    }
}

/// Everything in view of a terminal, clipped to where it can be seen there. A
/// terminal with cells of another size than panes go by gets images scaled to the
/// cells they should cover.
fn wanted(session: &Session, view: Rect, outer_cell: (u16, u16)) -> Vec<(ImageKey, Placement)> {
    let (width, height) = session.cell_size();
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let cell_size = (u32::from(width), u32::from(height));
    let same_cells = outer_cell == (width, height);
    let mut wanted = Vec::new();
    for (id, rect) in session.visible_panes() {
        let Some(pane) = session.panes.get(&id) else {
            continue;
        };
        let Some(part) = rect.intersection(view) else {
            continue;
        };
        let shown = Rect {
            x: part.x - view.x,
            y: part.y - view.y,
            ..part
        };
        let (cut_x, cut_y) = (i32::from(part.x - rect.x), i32::from(part.y - rect.y));
        for image in pane.terminal.image_placements() {
            let moved = ImagePlacement {
                col: image.col - cut_x,
                row: image.row - cut_y,
                ..image
            };
            if let Some(placement) = clip(&moved, shown, cell_size, same_cells) {
                wanted.push(((id, image.image, image.generation), placement));
            }
        }
    }
    wanted
}

/// One direction of an image, seen through one direction of a pane.
struct Span {
    /// The first cell of the image, relative to the pane.
    cell: i32,
    /// Where it starts within that cell, in pixels.
    offset: u32,
    /// Its size on the screen, in pixels.
    size: u32,
    /// The part of the image it shows, as its start and length.
    source: (u32, u32),
    /// Where the pane starts, in cells.
    pane_start: u16,
    /// How long the pane is, in cells.
    pane_len: u16,
    cell_size: u32,
}

/// What is left of a [`Span`].
struct Clipped {
    /// The first cell on the screen.
    cell: u16,
    offset: u32,
    source: (u32, u32),
    /// How many cells it takes up, rounded, for when it has to be scaled.
    cells: u32,
    /// Its size on the screen, in pixels.
    size: u32,
    /// Whether that is a whole number of cells.
    whole: bool,
}

impl Clipped {
    /// Whether scaling to this size makes the other direction come out at its
    /// size too, by the proportions of what is shown of the image.
    fn sets_size_of(&self, other: &Self) -> bool {
        let follows = u64::from(other.source.1) * u64::from(self.size) / u64::from(self.source.1);
        follows.abs_diff(u64::from(other.size)) <= 1
    }
}

impl Span {
    fn clip(&self) -> Option<Clipped> {
        let cell_size = i64::from(self.cell_size);
        let scaled_size = i64::from(self.size);
        let start = i64::from(self.cell) * cell_size + i64::from(self.offset);
        let first = start.max(0);
        let last = (start + scaled_size).min(i64::from(self.pane_len) * cell_size);
        if first >= last || scaled_size == 0 {
            return None;
        }
        let (source_start, source_len) = (i64::from(self.source.0), i64::from(self.source.1));
        let in_source = |pixel: i64| (pixel - start) * source_len / scaled_size;
        let (from, to) = (in_source(first), in_source(last));
        Some(Clipped {
            cell: u16::try_from(first / cell_size + i64::from(self.pane_start)).ok()?,
            offset: u32::try_from(first % cell_size).ok()?,
            source: (
                u32::try_from(source_start + from).ok()?,
                // Scaling up can leave less than a pixel of the image in view.
                u32::try_from((to - from).max(1)).ok()?,
            ),
            cells: u32::try_from(((last - first + cell_size / 2) / cell_size).max(1)).ok()?,
            size: u32::try_from(last - first).ok()?,
            whole: (last - first) % cell_size == 0,
        })
    }
}

/// The cells to scale to. The protocol only knows whole cells, so a size that is
/// not has to come from the other direction, by the proportions of the image.
/// That leaves a scaled image a bit off in size only where neither is whole, or
/// where it is not scaled the same in both directions.
fn scale_to(horizontal: &Clipped, vertical: &Clipped) -> (u32, u32) {
    if vertical.whole || horizontal.whole {
        if !vertical.whole && horizontal.sets_size_of(vertical) {
            return (horizontal.cells, 0);
        }
        if !horizontal.whole && vertical.sets_size_of(horizontal) {
            return (0, vertical.cells);
        }
    }
    (horizontal.cells, vertical.cells)
}

/// Clips an image to a pane. A pane starts and ends at the edge of a cell, so
/// where it cuts the image, nothing else needs to change. Without `same_cells`,
/// pixels mean something else on the outer terminal, so what counts is the cells
/// an image covers.
fn clip(
    image: &ImagePlacement,
    rect: Rect,
    cell_size: (u32, u32),
    same_cells: bool,
) -> Option<Placement> {
    let horizontal = Span {
        cell: image.col,
        offset: image.cell_offset.0,
        size: image.size.0,
        source: (image.source.x, image.source.width),
        pane_start: rect.x,
        pane_len: rect.width,
        cell_size: cell_size.0,
    }
    .clip()?;
    let vertical = Span {
        cell: image.row,
        offset: image.cell_offset.1,
        size: image.size.1,
        source: (image.source.y, image.source.height),
        pane_start: rect.y,
        pane_len: rect.height,
        cell_size: cell_size.1,
    }
    .clip()?;
    let own_size = same_cells && image.size == (image.source.width, image.source.height);
    let offset = (horizontal.offset, vertical.offset);
    Some(Placement {
        x: horizontal.cell,
        y: vertical.cell,
        offset: if same_cells { offset } else { (0, 0) },
        source: ImageRect {
            x: horizontal.source.0,
            y: vertical.source.0,
            width: horizontal.source.1,
            height: vertical.source.1,
        },
        cells: match (own_size, same_cells) {
            (true, _) => (0, 0),
            (false, true) => scale_to(&horizontal, &vertical),
            (false, false) => (horizontal.cells, vertical.cells),
        },
        z: image.z,
    })
}

fn push_command(out: &mut Vec<u8>, keys: &str) {
    out.extend_from_slice(format!("\x1b_G{keys}\x1b\\").as_bytes());
}

/// `C=1` keeps the cursor where it is, or the terminal would scroll to fit an
/// image that ends on the last row.
fn push_placement(out: &mut Vec<u8>, image: u32, id: u32, placement: &Placement) {
    let ImageRect {
        x,
        y,
        width,
        height,
    } = placement.source;
    let (offset_x, offset_y) = placement.offset;
    let z = placement.z;
    let scale = match placement.cells {
        (0, 0) => String::new(),
        (cols, 0) => format!(",c={cols}"),
        (0, rows) => format!(",r={rows}"),
        (cols, rows) => format!(",c={cols},r={rows}"),
    };
    push_command(
        out,
        &format!(
            "a=p,i={image},p={id},x={x},y={y},w={width},h={height},X={offset_x},Y={offset_y},\
             z={z},C=1,q=2{scale}"
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (u32, u32) = (10, 20);

    /// On a terminal with the same cells as the pane, which is most of them.
    fn clip(image: &ImagePlacement, rect: Rect, cell_size: (u32, u32)) -> Option<Placement> {
        super::clip(image, rect, cell_size, true)
    }

    /// A pane that starts at column 5, row 2, of 20 columns by 10 rows.
    const PANE: Rect = Rect {
        x: 5,
        y: 2,
        width: 20,
        height: 10,
    };

    /// An image of 40 by 60 pixels at its own size, which is 4 by 3 cells.
    fn image(col: i32, row: i32) -> ImagePlacement {
        ImagePlacement {
            col,
            row,
            size: (40, 60),
            source: ImageRect {
                x: 0,
                y: 0,
                width: 40,
                height: 60,
            },
            ..ImagePlacement::default()
        }
    }

    #[test]
    fn an_image_inside_of_its_pane_only_moves() {
        let placement = clip(&image(1, 1), PANE, CELL).unwrap();
        assert_eq!((placement.x, placement.y), (6, 3));
        assert_eq!(placement.source, image(1, 1).source);
        assert_eq!(placement.offset, (0, 0));
        assert_eq!(placement.cells, (0, 0));
    }

    #[test]
    fn what_scrolled_off_the_top_is_cut_off() {
        let placement = clip(&image(0, -1), PANE, CELL).unwrap();
        assert_eq!((placement.x, placement.y), (5, 2));
        assert_eq!(placement.source.y, 20);
        assert_eq!(placement.source.height, 40);
    }

    #[test]
    fn the_edges_of_the_pane_cut_off_the_rest() {
        let placement = clip(&image(18, 9), PANE, CELL).unwrap();
        assert_eq!((placement.x, placement.y), (23, 11));
        assert_eq!((placement.source.x, placement.source.y), (0, 0));
        assert_eq!(placement.source.width, 20);
        assert_eq!(placement.source.height, 20);
        let placement = clip(&image(-2, 0), PANE, CELL).unwrap();
        assert_eq!(placement.source.x, 20);
        assert_eq!(placement.source.width, 20);
    }

    #[test]
    fn an_image_out_of_view_is_not_placed() {
        assert_eq!(clip(&image(0, -3), PANE, CELL), None);
        assert_eq!(clip(&image(0, 10), PANE, CELL), None);
        assert_eq!(clip(&image(-4, 0), PANE, CELL), None);
        assert_eq!(clip(&image(20, 0), PANE, CELL), None);
    }

    #[test]
    fn an_offset_within_the_cell_stays_until_the_edge_cuts_it_off() {
        let mut offset = image(0, 0);
        offset.cell_offset = (3, 7);
        assert_eq!(clip(&offset, PANE, CELL).unwrap().offset, (3, 7));

        // One cell up from there, it starts 13 pixels above the pane.
        offset.row = -1;
        let placement = clip(&offset, PANE, CELL).unwrap();
        assert_eq!(placement.offset, (3, 0));
        assert_eq!(placement.source.y, 13);
        assert_eq!(placement.source.height, 47);
    }

    #[test]
    fn a_scaled_image_stays_scaled_where_it_is_cut() {
        // Twice its own size, so that one pixel of the image is two on the screen.
        let mut scaled = image(0, -1);
        scaled.size = (80, 120);
        let placement = clip(&scaled, PANE, CELL).unwrap();
        assert_eq!(placement.source.y, 10);
        assert_eq!(placement.source.height, 50);
        assert_eq!(placement.cells, (8, 5));
    }

    #[test]
    fn other_cells_keep_an_image_on_the_cells_it_covers() {
        let mut offset = image(1, 1);
        offset.cell_offset = (3, 7);
        let placement = super::clip(&offset, PANE, CELL, false).unwrap();
        assert_eq!((placement.x, placement.y), (6, 3));
        assert_eq!(placement.cells, (4, 3));
        assert_eq!(placement.offset, (0, 0));
    }

    #[test]
    fn a_sliver_of_an_image_scaled_up_is_still_a_pixel() {
        let mut scaled = image(0, 0);
        scaled.source.height = 1;
        scaled.size.1 = 400;
        scaled.row = -19;
        let placement = clip(&scaled, PANE, CELL).unwrap();
        assert_eq!(placement.source.height, 1);
    }

    fn sequences(out: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(out);
        let parts = text.split("\x1b_G").skip(1);
        parts.map(|part| part.replace("\x1b\\", "")).collect()
    }

    #[test]
    fn pixels_go_out_in_pieces_of_the_size_the_protocol_allows() {
        let data = vec![7; CHUNK + 1];
        let pixels = ImagePixels {
            width: 1025,
            height: 1,
            format: 24,
            data: &data,
        };
        let mut out = Vec::new();
        Transfer::new(9, &pixels).send(usize::MAX, &mut out);
        let sent = sequences(&out);
        assert_eq!(sent.len(), 2);
        assert!(sent[0].starts_with("a=t,f=24,s=1025,v=1,i=9,q=2,m=1;"));
        assert!(sent[1].starts_with("q=2,m=0;"));
        let (_, payload) = sent[0].split_once(';').unwrap();
        assert_eq!(payload.len(), 4096);
    }

    #[test]
    fn an_empty_image_sends_nothing() {
        let pixels = ImagePixels {
            width: 0,
            height: 0,
            format: 24,
            data: &[],
        };
        let mut out = Vec::new();
        let mut transfer = Transfer::new(9, &pixels);
        transfer.send(FRAME_PIXELS, &mut out);
        assert_eq!(out, b"");
        assert!(transfer.finished());
    }

    #[test]
    fn a_large_image_takes_several_frames() {
        let data = vec![7; FRAME_PIXELS + CHUNK];
        let pixels = ImagePixels {
            width: u32::try_from(data.len() / 3).unwrap(),
            height: 1,
            format: 24,
            data: &data,
        };
        let mut images = Images::default();
        images.transfers.push_back(Transfer::new(9, &pixels));

        let mut out = Vec::new();
        images.send_pixels(&mut out);
        let first = sequences(&out);
        assert_eq!(first.len(), FRAME_PIXELS / CHUNK);
        assert!(first[0].starts_with("a=t,"));
        assert!(first.iter().all(|sequence| sequence.contains("m=1;")));
        assert!(images.transferring());

        let mut out = Vec::new();
        images.send_pixels(&mut out);
        let rest = sequences(&out);
        assert_eq!(rest.len(), 1);
        assert!(rest[0].starts_with("q=2,m=0;"));
        assert!(!images.transferring());
    }

    /// A strip of an image scaled down, as a placeholder cell gives it: all
    /// columns of it, and part of the height of a row.
    #[test]
    fn a_size_of_part_of_a_cell_follows_from_the_other_direction() {
        let strip = ImagePlacement {
            col: 0,
            row: 0,
            cell_offset: (0, 12),
            size: (60, 9),
            source: ImageRect {
                x: 0,
                y: 0,
                width: 90,
                height: 14,
            },
            ..ImagePlacement::default()
        };
        let cell = (10, 21);
        let placement = clip(&strip, PANE, cell).unwrap();
        assert_eq!(placement.cells, (6, 0));

        let sideways = ImagePlacement {
            cell_offset: (3, 0),
            size: (7, 21),
            source: ImageRect {
                x: 0,
                y: 0,
                width: 14,
                height: 42,
            },
            ..strip
        };
        assert_eq!(clip(&sideways, PANE, cell).unwrap().cells, (0, 1));
    }

    #[test]
    fn a_stretched_image_keeps_both_directions() {
        // Twice as wide and three times as high, so they do not follow.
        let stretched = ImagePlacement {
            cell_offset: (0, 12),
            size: (80, 9),
            source: ImageRect {
                x: 0,
                y: 0,
                width: 40,
                height: 3,
            },
            ..image(0, 0)
        };
        assert_eq!(clip(&stretched, PANE, (10, 21)).unwrap().cells, (8, 1));
    }

    #[test]
    fn placements_only_ask_for_scaling_when_they_need_it() {
        let placement = clip(&image(1, 1), PANE, CELL).unwrap();
        let mut out = Vec::new();
        push_placement(&mut out, 3, 4, &placement);
        assert_eq!(
            sequences(&out),
            ["a=p,i=3,p=4,x=0,y=0,w=40,h=60,X=0,Y=0,z=0,C=1,q=2"]
        );
        let scaled = Placement {
            cells: (2, 3),
            ..placement
        };
        let mut out = Vec::new();
        push_placement(&mut out, 3, 4, &scaled);
        assert!(sequences(&out)[0].ends_with(",q=2,c=2,r=3"));
    }

    #[test]
    fn ids_never_end_up_at_zero_which_the_protocol_takes_for_no_id() {
        let mut images = Images {
            last_id: u32::MAX,
            ..Images::default()
        };
        assert_eq!(images.next_id(), 1);
    }
}
