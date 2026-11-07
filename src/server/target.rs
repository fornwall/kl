//! Finding what a command is about, by tmux's rules for `-t` and `-s`. With a
//! single session, that comes down to windows and panes.

use super::layout::{self, PaneId};
use super::session::{SESSION_NAME, Session, Window};
use crate::command::Direction;

/// What a target has to come down to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Window,
    Pane,
}

/// A target split into `session:window.pane`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Parts<'a> {
    session: Option<&'a str>,
    window: Option<&'a str>,
    /// Only a window of this very name, as `=name` asks for.
    exact_window: bool,
    pane: Option<&'a str>,
    /// Written with a period, so it can only be a pane.
    pane_only: bool,
}

fn non_empty(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

impl<'a> Parts<'a> {
    fn of(target: &'a str, wanted: Wanted) -> Self {
        let (session, rest) = match target.split_once(':') {
            Some((session, rest)) => (Some(session), rest),
            None => (None, target),
        };
        let mut parts = Self {
            session: session.and_then(non_empty),
            ..Self::default()
        };
        if let Some((window, pane)) = rest.split_once('.') {
            parts.window = Some(window);
            parts.pane = Some(pane);
            parts.pane_only = true;
        } else if session.is_some() {
            parts.window = Some(rest);
        } else {
            match (rest.chars().next(), wanted) {
                (Some('$'), _) => parts.session = Some(rest),
                (Some('@'), _) | (_, Wanted::Window) if !rest.starts_with('%') => {
                    parts.window = Some(rest);
                }
                _ => parts.pane = Some(rest),
            }
        }
        if let Some(window) = parts.window.and_then(|window| window.strip_prefix('=')) {
            parts.window = Some(window);
            parts.exact_window = true;
        }
        parts.window = parts.window.and_then(non_empty);
        parts.pane = parts.pane.and_then(non_empty);
        parts
    }
}

/// Whether text matches a shell pattern of `*` and `?`.
fn glob_matches(pattern: &[char], text: &[char]) -> bool {
    match (pattern.split_first(), text.split_first()) {
        (None, None) => true,
        (Some(('*', rest)), _) => {
            glob_matches(rest, text) || (!text.is_empty() && glob_matches(pattern, &text[1..]))
        }
        (Some((wanted, rest)), Some((c, tail))) if *wanted == '?' || wanted == c => {
            glob_matches(rest, tail)
        }
        _ => false,
    }
}

/// The only one of some candidates, if there is exactly one.
fn only<T>(mut candidates: impl Iterator<Item = T>) -> Option<T> {
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

/// `+` or `-`, with an optional number of steps.
fn offset(spec: &str) -> Option<(bool, usize)> {
    let forward = match spec.chars().next()? {
        '+' => true,
        '-' => false,
        _ => return None,
    };
    let steps = spec.get(1..).filter(|steps| !steps.is_empty());
    let steps = steps.map_or(Some(1), |steps| {
        steps.parse().ok().filter(|steps| *steps > 0)
    });
    Some((forward, steps?))
}

/// Goes some steps from a position in a ring.
const fn stepped(position: usize, count: usize, forward: bool, steps: usize) -> usize {
    let steps = steps % count;
    if forward {
        (position + steps) % count
    } else {
        (position + count - steps) % count
    }
}

impl Session {
    /// The pane a command is about without a target: the one it was run from,
    /// and otherwise the active one.
    pub fn current_pane(&self) -> Option<PaneId> {
        self.origin
            .filter(|id| self.panes.contains_key(id))
            .or_else(|| self.active_id())
    }

    fn current_window(&self) -> Option<&Window> {
        self.current_pane()
            .and_then(|id| self.window_of(id))
            .or_else(|| self.window())
    }

    pub fn check_session(session: Option<&str>) -> Result<(), String> {
        match session {
            Some(name) if !(name == "$0" || SESSION_NAME.starts_with(name)) => {
                Err(format!("can't find session: {name}"))
            }
            _ => Ok(()),
        }
    }

    /// A window with a name, or a pattern for one. As in tmux, more than one
    /// match does not make for a window.
    fn window_named(&self, spec: &str, exact: bool) -> Option<&Window> {
        let named = |matches: &dyn Fn(&str) -> bool| {
            only(self.windows.iter().filter(|window| matches(&window.name)))
        };
        let same = self.windows.iter().filter(|window| window.name == spec);
        if same.clone().next().is_some() {
            return only(same);
        }
        if exact {
            return None;
        }
        if self.windows.iter().any(|w| w.name.starts_with(spec)) {
            return named(&|name| name.starts_with(spec));
        }
        let pattern: Vec<char> = spec.chars().collect();
        named(&|name| glob_matches(&pattern, &name.chars().collect::<Vec<_>>()))
    }

    fn window_by_spec(&self, spec: &str, exact: bool) -> Option<&Window> {
        if let Some(id) = spec.strip_prefix('@') {
            let id: u32 = id.parse().ok()?;
            return self.windows.iter().find(|window| window.id == id);
        }
        let spec = match spec {
            "{start}" => "^",
            "{end}" => "$",
            "{last}" => "!",
            "{next}" => "+",
            "{previous}" => "-",
            spec => spec,
        };
        if !exact {
            if let Some((forward, steps)) = offset(spec) {
                let current = self.current_window()?;
                let position = self.windows.iter().position(|w| w.index == current.index)?;
                let target = stepped(position, self.windows.len(), forward, steps);
                return self.windows.get(target);
            }
            match spec {
                "!" => return self.last.and_then(|index| self.window_at(index)),
                "^" => return self.windows.first(),
                "$" => return self.windows.last(),
                _ => {}
            }
        }
        spec.parse()
            .ok()
            .and_then(|index| self.window_at(index))
            .or_else(|| self.window_named(spec, exact))
    }

    /// The pane at a corner or in the middle of an edge of a window.
    fn pane_at_edge(&self, window: &Window, spec: &str) -> Option<PaneId> {
        let area = self.area();
        let (right, bottom) = (area.width.saturating_sub(1), area.height.saturating_sub(1));
        let (x, y) = (area.width / 2, area.height / 2);
        let (x, y) = match spec {
            "top" => (x, 0),
            "bottom" => (x, bottom),
            "left" => (0, y),
            "right" => (right, y),
            "top-left" => (0, 0),
            "top-right" => (right, 0),
            "bottom-left" => (0, bottom),
            "bottom-right" => (right, bottom),
            _ => return None,
        };
        // A pane has the border on its right and below it to itself.
        let at = |pane: &&(PaneId, layout::Rect)| {
            let rect = pane.1;
            let grown = layout::Rect {
                width: rect.width.saturating_add(1),
                height: rect.height.saturating_add(1),
                ..rect
            };
            grown.contains(x, y)
        };
        let panes = window.root.panes(area);
        panes.iter().find(at).map(|(id, _)| *id)
    }

    fn pane_in(&self, window: &Window, spec: &str) -> Option<PaneId> {
        let area = self.area();
        let panes = window.root.panes(area);
        let spec = match spec {
            "{last}" => "!",
            "{next}" => "+",
            "{previous}" => "-",
            spec => spec
                .strip_prefix('{')
                .and_then(|spec| spec.strip_suffix('}'))
                .unwrap_or(spec),
        };
        let direction = match spec {
            "up-of" => Some(Direction::Up),
            "down-of" => Some(Direction::Down),
            "left-of" => Some(Direction::Left),
            "right-of" => Some(Direction::Right),
            _ => None,
        };
        if let Some(direction) = direction {
            return layout::neighbor(&panes, window.active, direction, area);
        }
        if spec == "!" {
            return window.last_active;
        }
        if let Some((forward, steps)) = offset(spec) {
            let position = panes.iter().position(|(id, _)| *id == window.active)?;
            let target = stepped(position, panes.len(), forward, steps);
            return panes.get(target).map(|(id, _)| *id);
        }
        let index = spec.parse().ok();
        index
            .and_then(|index: usize| panes.get(index).map(|(id, _)| *id))
            .or_else(|| self.pane_at_edge(window, spec))
    }

    /// The pane a target is about. Without one, that is the current pane.
    pub fn find_pane(&self, target: Option<&str>) -> Result<PaneId, String> {
        let current = || {
            self.current_pane()
                .ok_or_else(|| "no current pane".to_owned())
        };
        let Some(target) = target else {
            return current();
        };
        let parts = Parts::of(target, Wanted::Pane);
        Self::check_session(parts.session)?;
        let window = parts.window.map(|spec| {
            self.window_by_spec(spec, parts.exact_window)
                .ok_or_else(|| format!("can't find window: {spec}"))
        });
        let window = window.transpose()?;
        let Some(spec) = parts.pane else {
            return window.map_or_else(current, |window| Ok(window.active));
        };
        let not_found = || format!("can't find pane: {spec}");
        if let Some(id) = spec.strip_prefix('%') {
            let id = id.parse().ok().filter(|id| {
                let in_window = window.is_none_or(|window| window.root.contains(*id));
                in_window && self.panes.contains_key(id)
            });
            return id.ok_or_else(not_found);
        }
        let in_window = window.or_else(|| self.current_window());
        let found = in_window.and_then(|window| self.pane_in(window, spec));
        let as_window = || {
            let window = self.window_by_spec(spec, false);
            window
                .filter(|_| !parts.pane_only)
                .map(|window| window.active)
        };
        found.or_else(as_window).ok_or_else(not_found)
    }

    /// The pane a source is about. Without one, that is the marked pane if
    /// there is one, and the current pane otherwise.
    pub fn find_source(&self, source: Option<&str>) -> Result<PaneId, String> {
        match (source, self.marked_pane()) {
            (None, Some(marked)) => Ok(marked),
            _ => self.find_pane(source),
        }
    }

    /// The window a source is about, which is that of the marked pane if there
    /// is one, and the current one otherwise.
    pub fn find_source_window(&self, source: Option<&str>) -> Result<u32, String> {
        let marked = self.marked_pane().and_then(|id| self.window_of(id));
        match (source, marked) {
            (None, Some(window)) => Ok(window.index),
            _ => self.find_window(source),
        }
    }

    /// The pane before or after a pane of its window, which wraps around.
    pub fn pane_next_to(&self, id: PaneId, forward: bool) -> Option<PaneId> {
        let panes = self.window_of(id)?.root.panes(self.area());
        let position = panes.iter().position(|(pane, _)| *pane == id)?;
        let target = stepped(position, panes.len(), forward, 1);
        panes.get(target).map(|(pane, _)| *pane)
    }

    /// The index of the window a target is about. Without one, that is the
    /// current window.
    pub fn find_window(&self, target: Option<&str>) -> Result<u32, String> {
        let current = || {
            let window = self.current_window();
            window
                .map(|window| window.index)
                .ok_or_else(|| "no current window".to_owned())
        };
        let Some(target) = target else {
            return current();
        };
        let parts = Parts::of(target, Wanted::Window);
        Self::check_session(parts.session)?;
        if let Some(pane) = parts.pane.filter(|pane| pane.starts_with('%')) {
            let id = self.find_pane(Some(pane))?;
            let window = self.window_of(id).map(|window| window.index);
            return window.ok_or_else(|| format!("can't find pane: {pane}"));
        }
        let Some(spec) = parts.window else {
            return current();
        };
        self.window_by_spec(spec, parts.exact_window)
            .map(|window| window.index)
            .ok_or_else(|| format!("can't find window: {spec}"))
    }

    /// Where a target for a new window puts it, if it says anything about that.
    /// The index can be taken already.
    pub fn find_index(&self, target: &str) -> Result<Option<u32>, String> {
        let parts = Parts::of(target, Wanted::Window);
        Self::check_session(parts.session)?;
        let Some(spec) = parts.window else {
            return Ok(None);
        };
        // Parsing takes a plus sign too, which makes this an offset instead.
        if let Some(index) = spec.parse().ok().filter(|_| !spec.starts_with('+')) {
            return Ok(Some(index));
        }
        let window = self.window_by_spec(spec, parts.exact_window);
        let window = window.ok_or_else(|| format!("can't find window: {spec}"))?;
        Ok(Some(window.index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(target: &str) -> Parts<'_> {
        Parts::of(target, Wanted::Pane)
    }

    fn window(target: &str) -> Parts<'_> {
        Parts::of(target, Wanted::Window)
    }

    #[test]
    fn splits_targets_like_tmux() {
        let parts = pane("kl:1.2");
        assert_eq!(parts.session, Some("kl"));
        assert_eq!(parts.window, Some("1"));
        assert_eq!(parts.pane, Some("2"));

        assert_eq!(pane("1").pane, Some("1"));
        assert_eq!(pane("1").window, None);
        assert_eq!(window("1").window, Some("1"));
        assert_eq!(pane("@3").window, Some("@3"));
        assert_eq!(window("%3").pane, Some("%3"));
        assert_eq!(pane("$0").session, Some("$0"));

        assert_eq!(pane("1.").window, Some("1"));
        assert_eq!(pane("1.").pane, None);
        assert_eq!(pane(":1").window, Some("1"));
        assert_eq!(pane(":").window, None);
        assert_eq!(pane(":.+").pane, Some("+"));
        assert!(pane(":.+").pane_only);
        assert_eq!(pane(".1").window, None);
        assert!(pane(".1").pane_only);
    }

    #[test]
    fn reads_exact_names() {
        let parts = window("=logs");
        assert_eq!(parts.window, Some("logs"));
        assert!(parts.exact_window);
        assert!(pane(":=1").exact_window);
        assert!(!pane("=1").exact_window);
    }

    fn matches(pattern: &str, text: &str) -> bool {
        let pattern: Vec<char> = pattern.chars().collect();
        let text: Vec<char> = text.chars().collect();
        glob_matches(&pattern, &text)
    }

    #[test]
    fn matches_patterns() {
        assert!(matches("a*", "abc"));
        assert!(matches("*c", "abc"));
        assert!(matches("a*c", "abc"));
        assert!(matches("a?c", "abc"));
        assert!(matches("*", ""));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("b*", "abc"));
        assert!(!matches("abc", "ab"));
    }

    #[test]
    fn reads_offsets() {
        assert_eq!(offset("+"), Some((true, 1)));
        assert_eq!(offset("-"), Some((false, 1)));
        assert_eq!(offset("+3"), Some((true, 3)));
        assert_eq!(offset("-x"), None);
        assert_eq!(offset("3"), None);
    }

    #[test]
    fn steps_around_a_ring() {
        assert_eq!(stepped(2, 3, true, 1), 0);
        assert_eq!(stepped(0, 3, false, 1), 2);
        assert_eq!(stepped(1, 3, false, 7), 0);
    }
}
