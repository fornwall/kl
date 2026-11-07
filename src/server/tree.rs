//! The list behind `choose-tree` and `find-window`, after tmux's tree mode:
//! windows, with panes below them.

use super::Server;
use super::format::Context;
use super::layout::PaneId;
use super::pane::Pane;
use super::session::{Session, Window};
use super::target::glob_matches;
use super::ui::{List, ListRow};
use crate::command::{Command, FindWindow};

fn window_row(session: &Session, window: &Window) -> ListRow {
    let text = Context::of_window(session, window).expand("#I: #W#F#{?pane_title, \"#T\",}");
    let target = Some(format!("@{}", window.id));
    ListRow::new(text, Command::SelectWindow { target })
}

fn pane_row(session: &Session, id: PaneId) -> ListRow {
    let format = "    #P: #{pane_current_command}#{?pane_active,*,}#{?pane_marked,M,}#{?pane_title, \"#T\",}";
    let text = Context::of_pane(session, id).expand(format);
    let target = Some(format!("%{id}"));
    ListRow::new(text, Command::GoToPane { target }).nested()
}

/// A window with its panes folded up below it.
fn folded_window_row(session: &Session, window: &Window) -> ListRow {
    let panes = window.root.panes(session.area());
    let rows = panes.iter().map(|(id, _)| pane_row(session, *id));
    window_row(session, window).with_folded(rows.collect())
}

/// The rows a pane shows, as text without what is empty at the end. Those are
/// the rows of the screen rather than the view, which copy mode can move away.
fn shown_rows(pane: &Pane) -> impl Iterator<Item = Vec<char>> {
    let bar = pane.terminal.scrollbar();
    let row = |at: u64| u32::try_from(at).unwrap_or(u32::MAX);
    let first = bar.map_or(0, |bar| row(bar.history()));
    let end = bar.map_or(0, |bar| row(bar.total));
    (first..end).map(|y| {
        let line = pane.terminal.screen_line(y);
        let cells = line.cells.iter().take(usize::from(line.length()));
        cells.copied().filter(|c| *c != '\0').collect()
    })
}

/// What `find-window` goes by. As in tmux, a pattern is about part of the text.
struct Finder<'a> {
    find: &'a FindWindow,
    pattern: Vec<char>,
}

impl<'a> Finder<'a> {
    fn new(find: &'a FindWindow) -> Self {
        let pattern = format!("*{}*", find.pattern);
        Self {
            find,
            pattern: fold(find, pattern.chars()),
        }
    }

    fn matches(&self, text: impl Iterator<Item = char>) -> bool {
        glob_matches(&self.pattern, &fold(self.find, text))
    }

    fn finds(&self, window: &Window, pane: &Pane) -> bool {
        let searched = self.find.searched;
        (searched.name && self.matches(window.name.chars()))
            || (searched.title && self.matches(pane.terminal.title().chars()))
            || (searched.content && shown_rows(pane).any(|row| self.matches(row.into_iter())))
    }
}

/// Text the way it gets compared.
fn fold(find: &FindWindow, text: impl Iterator<Item = char>) -> Vec<char> {
    if find.ignore_case {
        text.flat_map(char::to_lowercase).collect()
    } else {
        text.collect()
    }
}

impl Server {
    /// The window chooser, on the row of the current window.
    pub fn choose_tree(&mut self) {
        let session = &self.session;
        let windows = session.windows.iter();
        let rows = windows.map(|window| folded_window_row(session, window));
        let rows = rows.collect();
        let current = session
            .windows
            .iter()
            .position(|window| window.index == session.current);
        let list = List::new(rows, current.unwrap_or(0));
        self.client().ui.list = Some(list);
    }

    /// Lists the panes that match below their windows, on the first of them.
    /// As in tmux, there is no list of nothing: that shows all windows.
    pub fn find_window(&mut self, find: &FindWindow) {
        let session = &self.session;
        let finder = Finder::new(find);
        let area = session.area();
        let mut rows = Vec::new();
        for window in &session.windows {
            let panes = window.root.panes(area);
            let found: Vec<PaneId> = panes
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| {
                    let pane = session.panes.get(id);
                    pane.is_some_and(|pane| finder.finds(window, pane))
                })
                .collect();
            if !found.is_empty() {
                rows.push(window_row(session, window).unfolded());
                rows.extend(found.into_iter().map(|id| pane_row(session, id)));
            }
        }
        if rows.is_empty() {
            self.client()
                .ui
                .show_message("filter: no matches".to_owned());
            self.choose_tree();
        } else {
            self.client().ui.list = Some(List::new(rows, 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, ignore_case: bool) -> FindWindow {
        FindWindow {
            pattern: pattern.to_owned(),
            ignore_case,
            ..FindWindow::default()
        }
    }

    #[test]
    fn a_pattern_is_about_part_of_the_text() {
        let needle = find("needle", false);
        let finder = Finder::new(&needle);
        assert!(finder.matches("a needle here".chars()));
        assert!(!finder.matches("a NEEDLE here".chars()));
        assert!(!finder.matches("hay".chars()));
    }

    #[test]
    fn patterns_can_ignore_case_and_have_wildcards() {
        let needle = find("N?edle*x", true);
        let finder = Finder::new(&needle);
        assert!(finder.matches("the needle in x".chars()));
        assert!(finder.matches("NOEDLEX".chars()));
        assert!(!finder.matches("needle".chars()));
    }
}
