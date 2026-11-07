//! The hyperlinks in a viewport, which the render state leaves out.
//!
//! A cell's URI is only to be had from the live terminal, while the render state
//! stays behind during a hold. Taking a copy whenever the render state is updated
//! keeps text and links from different moments from being paired up.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::Cell;

/// The fewest links worth looking through for ones to forget.
const MIN_LIMIT: usize = 256;

/// Never used twice, so that comparing cells tells links apart wherever they
/// come from. Wrapping around would take billions of different links.
static NEXT_LINK: AtomicU32 = AtomicU32::new(1);

/// One URI in one terminal, however many cells and rows it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Link(NonZeroU32);

impl Link {
    fn new() -> Self {
        let number = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
        Self(NonZeroU32::new(number).unwrap_or(NonZeroU32::MIN))
    }

    pub const fn number(self) -> u32 {
        self.0.get()
    }
}

/// Adjacent cells of a row with the same URI.
struct Run {
    start: u16,
    end: u16,
    link: Link,
    uri: Rc<[u8]>,
}

#[derive(Default)]
pub struct Links {
    /// The runs of each row of the viewport, from left to right.
    rows: Vec<Vec<Run>>,
    by_uri: BTreeMap<Rc<[u8]>, Link>,
    by_link: BTreeMap<Link, Rc<[u8]>>,
    /// How many links to know before looking for ones to forget.
    limit: usize,
}

impl Links {
    /// How many links are kept.
    #[cfg(test)]
    pub fn known(&self) -> usize {
        self.by_link.len()
    }

    pub fn uri(&self, link: Link) -> Option<&[u8]> {
        self.by_link.get(&link).map(|uri| &**uri)
    }

    /// Forgets the links of a row that is about to be read again.
    pub fn clear_row(&mut self, y: usize) {
        if let Some(row) = self.rows.get_mut(y) {
            row.clear();
        }
    }

    /// Records the URI of a cell. Cells of a row must come from left to right.
    pub fn add(&mut self, x: u16, y: usize, uri: &[u8]) {
        if self.rows.len() <= y {
            self.rows.resize_with(y + 1, Vec::new);
        }
        if let Some(run) = self.rows.get_mut(y).and_then(|row| row.last_mut())
            && run.end == x
            && *run.uri == *uri
        {
            run.end += 1;
            return;
        }
        let (link, uri) = self.intern(uri);
        if let Some(row) = self.rows.get_mut(y) {
            row.push(Run {
                start: x,
                end: x + 1,
                link,
                uri,
            });
        }
    }

    fn intern(&mut self, uri: &[u8]) -> (Link, Rc<[u8]>) {
        if let Some((uri, link)) = self.by_uri.get_key_value(uri) {
            return (*link, Rc::clone(uri));
        }
        let (link, uri) = (Link::new(), Rc::<[u8]>::from(uri));
        self.by_uri.insert(Rc::clone(&uri), link);
        self.by_link.insert(link, Rc::clone(&uri));
        (link, uri)
    }

    /// Forgets the links that left the viewport, once enough of them piled up.
    /// A link that comes back before that keeps its number, so that scrolling
    /// back and forth finds nothing changed.
    pub fn forget_unused(&mut self) {
        if self.by_link.len() < self.limit.max(MIN_LIMIT) {
            return;
        }
        let used: BTreeSet<Link> = self.rows.iter().flatten().map(|run| run.link).collect();
        self.by_uri.retain(|_, link| used.contains(link));
        self.by_link.retain(|link, _| used.contains(link));
        self.limit = 2 * used.len();
    }

    /// Sets the links of a row of cells.
    pub fn apply(&self, y: usize, cells: &mut [Cell]) {
        for run in self.rows.get(y).into_iter().flatten() {
            let range = usize::from(run.start)..usize::from(run.end);
            for cell in cells.get_mut(range).into_iter().flatten() {
                cell.link = Some(run.link);
            }
        }
    }
}
