//! tmux's format language, or the part of it people actually use: variables and
//! their short forms, conditions, comparisons and the common modifiers. Unlike
//! in tmux, `s/from/to/` replaces plain text rather than a regular expression.

use std::path::Path;

use super::client::Client;
use super::layout::PaneId;
use super::pane::Pane;
use super::session::{SESSION_NAME, Session, Window};
use crate::{ghostty, sys};

/// Finds the value of a variable.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The variable behind a short form such as `#W`.
const fn short_form(c: char) -> Option<&'static str> {
    match c {
        'D' => Some("pane_id"),
        'F' => Some("window_flags"),
        'H' => Some("host"),
        'I' => Some("window_index"),
        'P' => Some("pane_index"),
        'S' => Some("session_name"),
        'T' => Some("pane_title"),
        'W' => Some("window_name"),
        _ => None,
    }
}

/// Finds the first of some characters that is not inside a nested `#{...}`.
fn skip(text: &str, stops: &str) -> Option<usize> {
    let mut depth = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if c == '#' {
            let next = chars.peek().map(|(_, next)| *next);
            depth += i32::from(next == Some('{'));
            if next.is_some_and(|next| ",#{}:".contains(next)) {
                chars.next();
                continue;
            }
        }
        depth -= i32::from(c == '}');
        if depth == 0 && stops.contains(c) {
            return Some(index);
        }
    }
    None
}

/// Splits on the commas that are not inside a nested `#{...}`.
fn split_args(mut text: &str) -> Vec<&str> {
    let mut args = Vec::new();
    while let Some(comma) = skip(text, ",") {
        args.push(text.get(..comma).unwrap_or(""));
        text = text.get(comma + 1..).unwrap_or("");
    }
    args.push(text);
    args
}

pub fn is_true(value: &str) -> bool {
    !value.is_empty() && value != "0"
}

fn holds(condition: &str, vars: Lookup<'_>) -> bool {
    let value =
        vars(condition).or_else(|| condition.contains('#').then(|| expand(condition, vars)));
    value.is_some_and(|value| is_true(&value))
}

/// `#{?condition,then,else}`, where more pairs may come before the else.
fn conditional(text: &str, vars: Lookup<'_>) -> String {
    let args = split_args(text);
    let mut rest = args.as_slice();
    while let [condition, then, tail @ ..] = rest {
        if holds(condition, vars) {
            return expand(then, vars);
        }
        rest = tail;
    }
    rest.first()
        .map_or_else(String::new, |otherwise| expand(otherwise, vars))
}

/// `#{==:a,b}` and its relatives, which are 1 or 0.
fn comparison(operator: &str, operands: &str, vars: Lookup<'_>) -> Option<String> {
    let test: fn(&str, &str) -> bool = match operator {
        "==" => |a, b| a == b,
        "!=" => |a, b| a != b,
        "<" => |a, b| a < b,
        ">" => |a, b| a > b,
        "<=" => |a, b| a <= b,
        ">=" => |a, b| a >= b,
        "||" => |a, b| is_true(a) || is_true(b),
        "&&" => |a, b| is_true(a) && is_true(b),
        _ => return None,
    };
    let args = split_args(operands);
    let [left, right] = args.as_slice() else {
        return None;
    };
    let result = test(&expand(left, vars), &expand(right, vars));
    Some(u8::from(result).to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum Modifier<'a> {
    Basename,
    Dirname,
    /// The text itself instead of a variable of that name.
    Literal,
    Length,
    Width,
    /// The value is a format itself.
    Expand,
    Not,
    /// Keeps as many columns, from the end if negative.
    Truncate {
        limit: i32,
        marker: &'a str,
    },
    Replace {
        from: &'a str,
        to: &'a str,
    },
}

/// `=5`, `=-5` or `=/5/...`, without the equals sign.
fn parse_truncate(text: &str) -> Option<(Modifier<'_>, &str)> {
    if let Some(body) = text.strip_prefix('/') {
        let (limit, body) = body.split_once('/')?;
        let (marker, rest) = body
            .find([':', ';'])
            .and_then(|end| body.get(..end).zip(body.get(end..)))?;
        let marker = marker.strip_suffix('/').unwrap_or(marker);
        let limit = limit.parse().ok()?;
        return Some((Modifier::Truncate { limit, marker }, rest));
    }
    let end = text.find([':', ';'])?;
    let limit = text.get(..end)?.parse().ok()?;
    let marker = "";
    Some((Modifier::Truncate { limit, marker }, text.get(end..)?))
}

/// `/from/to/`, where any character can take the place of the slash.
fn parse_replace(text: &str) -> Option<(Modifier<'_>, &str)> {
    let mut chars = text.chars();
    let delimiter = chars.next()?;
    let (from, body) = chars.as_str().split_once(delimiter)?;
    let (to, rest) = body.split_once(delimiter)?;
    Some((Modifier::Replace { from, to }, rest))
}

fn parse_modifier(text: &str) -> Option<(Modifier<'_>, &str)> {
    let mut chars = text.chars();
    let first = chars.next()?;
    let rest = chars.as_str();
    let modifier = match first {
        'b' => Modifier::Basename,
        'd' => Modifier::Dirname,
        'l' => Modifier::Literal,
        'n' => Modifier::Length,
        'w' => Modifier::Width,
        'E' => Modifier::Expand,
        '!' => Modifier::Not,
        '=' => return parse_truncate(rest),
        's' => return parse_replace(rest),
        _ => return None,
    };
    Some((modifier, rest))
}

/// Splits the modifiers off an expression, if it starts with any.
fn parse_modifiers(expression: &str) -> Option<(Vec<Modifier<'_>>, &str)> {
    let mut modifiers = Vec::new();
    let mut rest = expression;
    loop {
        let (modifier, after) = parse_modifier(rest)?;
        modifiers.push(modifier);
        if let Some(value) = after.strip_prefix(':') {
            return Some((modifiers, value));
        }
        rest = after.strip_prefix(';')?;
    }
}

fn text_width(text: &str) -> usize {
    text.chars()
        .map(|c| usize::from(ghostty::char_width(c)))
        .sum()
}

/// Takes characters for as long as they fit in some columns.
fn fitting(chars: impl Iterator<Item = char>, columns: usize) -> String {
    let mut used = 0;
    chars
        .take_while(|c| {
            used += usize::from(ghostty::char_width(*c));
            used <= columns
        })
        .collect()
}

fn truncate(value: &str, limit: i32, marker: &str) -> String {
    let columns = usize::try_from(limit.unsigned_abs()).unwrap_or(usize::MAX);
    if text_width(value) <= columns {
        return value.to_owned();
    }
    if limit < 0 {
        let end: String = fitting(value.chars().rev(), columns)
            .chars()
            .rev()
            .collect();
        format!("{marker}{end}")
    } else {
        format!("{}{marker}", fitting(value.chars(), columns))
    }
}

fn dirname(path: &str) -> String {
    match Path::new(path).parent().map(Path::to_string_lossy) {
        Some(parent) if !parent.is_empty() => parent.into_owned(),
        Some(_) => ".".to_owned(),
        None => path.to_owned(),
    }
}

fn apply(modifier: &Modifier<'_>, value: String, vars: Lookup<'_>) -> String {
    match modifier {
        Modifier::Basename => Path::new(&value)
            .file_name()
            .map_or_else(|| value.clone(), |name| name.to_string_lossy().into_owned()),
        Modifier::Dirname => dirname(&value),
        Modifier::Literal | Modifier::Replace { from: "", .. } => value,
        Modifier::Length => value.chars().count().to_string(),
        Modifier::Width => text_width(&value).to_string(),
        Modifier::Expand => expand(&value, vars),
        Modifier::Not => u8::from(!is_true(&value)).to_string(),
        Modifier::Truncate { limit, marker } => truncate(&value, *limit, marker),
        Modifier::Replace { from, to } => value.replace(from, to),
    }
}

/// What a name stands for: a nested format, or else a variable.
fn value_of(name: &str, vars: Lookup<'_>) -> String {
    if name.contains("#{") {
        expand(name, vars)
    } else {
        vars(name).unwrap_or_default()
    }
}

/// What is between the braces of `#{...}`.
fn expression(text: &str, vars: Lookup<'_>) -> String {
    if let Some(condition) = text.strip_prefix('?') {
        return conditional(condition, vars);
    }
    if let Some(result) = text
        .split_once(':')
        .and_then(|(operator, operands)| comparison(operator, operands, vars))
    {
        return result;
    }
    let Some((modifiers, name)) = parse_modifiers(text) else {
        return value_of(text, vars);
    };
    let value = if modifiers.contains(&Modifier::Literal) {
        name.to_owned()
    } else {
        value_of(name, vars)
    };
    modifiers
        .iter()
        .fold(value, |value, modifier| apply(modifier, value, vars))
}

/// Expands a format, with empty text for variables that do not exist.
pub fn expand(template: &str, vars: Lookup<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('#') {
        out.push_str(rest.get(..start).unwrap_or(""));
        let hash = rest.get(start..).unwrap_or("");
        let mut after = hash.chars().skip(1);
        rest = match after.next() {
            Some('{') => {
                let Some(end) = skip(hash, "}") else {
                    out.push_str(hash);
                    return out;
                };
                out.push_str(&expression(hash.get(2..end).unwrap_or(""), vars));
                hash.get(end + 1..).unwrap_or("")
            }
            Some(c @ ('#' | ',' | '}')) => {
                out.push(c);
                hash.get(2..).unwrap_or("")
            }
            Some(c) => {
                if let Some(name) = short_form(c) {
                    out.push_str(&vars(name).unwrap_or_default());
                } else {
                    out.push('#');
                    out.push(c);
                }
                hash.get(1 + c.len_utf8()..).unwrap_or("")
            }
            None => {
                out.push('#');
                ""
            }
        };
    }
    out.push_str(rest);
    out
}

/// What a format can refer to: the session, and a window and pane in it.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub session: &'a Session,
    pub window: Option<&'a Window>,
    pub pane: Option<&'a Pane>,
    pub client: Option<&'a Client>,
}

fn flag(value: bool) -> String {
    u8::from(value).to_string()
}

impl<'a> Context<'a> {
    /// The current window and its active pane.
    pub fn current(session: &'a Session) -> Self {
        Self {
            session,
            window: session.window(),
            pane: session.active_pane(),
            client: None,
        }
    }

    pub fn of_pane(session: &'a Session, id: PaneId) -> Self {
        Self {
            session,
            window: session.window_of(id),
            pane: session.panes.get(&id),
            client: None,
        }
    }

    /// A window and its active pane.
    pub fn of_window(session: &'a Session, window: &'a Window) -> Self {
        Self {
            session,
            window: Some(window),
            pane: session.panes.get(&window.active),
            client: None,
        }
    }

    pub fn expand(&self, template: &str) -> String {
        expand(template, &|name| self.value(name))
    }

    /// Whether a format comes out as true, as in `-f`.
    pub fn holds(&self, format: &str) -> bool {
        is_true(&self.expand(format))
    }

    fn value(&self, name: &str) -> Option<String> {
        if let Some(value) = self.client.and_then(|client| client.value(name)) {
            return Some(value);
        }
        let session = self.session;
        Some(match name {
            "client_height" => session.size.rows.to_string(),
            "client_width" => session.size.cols.to_string(),
            "host" => sys::host_name()?,
            "host_short" => {
                let host = sys::host_name()?;
                host.split('.').next().unwrap_or(&host).to_owned()
            }
            "pid" => std::process::id().to_string(),
            "session_id" => "$0".to_owned(),
            "session_name" => SESSION_NAME.to_owned(),
            "session_windows" => session.windows.len().to_string(),
            "version" => env!("CARGO_PKG_VERSION").to_owned(),
            _ => {
                return self
                    .window
                    .and_then(|window| self.window_value(window, name))
                    .or_else(|| self.pane.and_then(|pane| self.pane_value(pane, name)));
            }
        })
    }

    fn window_value(&self, window: &Window, name: &str) -> Option<String> {
        let session = self.session;
        let area = session.area();
        Some(match name {
            "window_active" => flag(window.index == session.current),
            "window_activity_flag" => flag(window.has_activity()),
            "window_bell_flag" => flag(window.bell),
            "window_flags" | "window_raw_flags" => session.window_flags(window),
            "window_height" => area.height.to_string(),
            "window_id" => format!("@{}", window.id),
            "window_index" => window.index.to_string(),
            "window_last_flag" => flag(session.last == Some(window.index)),
            "window_marked_flag" => {
                let marked = session.marked_pane();
                flag(marked.is_some_and(|id| window.root.contains(id)))
            }
            "window_name" => window.name.clone(),
            "window_panes" => window.root.panes(area).len().to_string(),
            "window_width" => area.width.to_string(),
            "window_zoomed_flag" => flag(window.zoomed),
            _ => return None,
        })
    }

    fn pane_value(&self, pane: &Pane, name: &str) -> Option<String> {
        let rect = pane.rect;
        let window = self.window?;
        Some(match name {
            "copy_cursor_x" => pane.copy_mode?.x.to_string(),
            "copy_cursor_y" => pane.copy_mode?.y.to_string(),
            "cursor_x" => pane.terminal.cursor()?.position?.0.to_string(),
            "cursor_y" => pane.terminal.cursor()?.position?.1.to_string(),
            "history_size" => pane.terminal.scrollbar()?.history().to_string(),
            "pane_active" => flag(window.active == pane.id),
            "pane_bottom" => (rect.y + rect.height.saturating_sub(1)).to_string(),
            "pane_current_command" => pane.foreground_name()?,
            "pane_current_path" => pane.working_directory()?.display().to_string(),
            "pane_height" => rect.height.to_string(),
            "pane_id" => format!("%{}", pane.id),
            "pane_dead" => "0".to_owned(),
            "pane_in_mode" => flag(pane.copy_mode.is_some()),
            "pane_index" => {
                let panes = window.root.panes(self.session.area());
                panes.iter().position(|(id, _)| *id == pane.id)?.to_string()
            }
            "pane_left" => rect.x.to_string(),
            "pane_marked" => flag(self.session.marked_pane() == Some(pane.id)),
            "pane_marked_set" => flag(self.session.marked_pane().is_some()),
            "pane_mode" => {
                if pane.copy_mode.is_some() {
                    "copy-mode".to_owned()
                } else {
                    String::new()
                }
            }
            "pane_pid" => pane.pid.to_string(),
            "pane_right" => (rect.x + rect.width.saturating_sub(1)).to_string(),
            "pane_start_command" => pane.start_command.clone()?,
            "pane_title" => pane.terminal.title(),
            "pane_top" => rect.y.to_string(),
            "pane_width" => rect.width.to_string(),
            "scroll_position" => {
                pane.copy_mode?;
                pane.terminal.scrollbar()?.lines_below().to_string()
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
#[expect(
    clippy::literal_string_with_formatting_args,
    reason = "these are formats, just not the ones of Rust"
)]
mod tests {
    use super::*;

    fn vars(name: &str) -> Option<String> {
        match name {
            "yes" => Some("1".to_owned()),
            "no" => Some("0".to_owned()),
            "empty" => Some(String::new()),
            "name" => Some("hello world".to_owned()),
            "path" => Some("/usr/local/bin".to_owned()),
            "wide" => Some("\u{4e16}\u{754c}x".to_owned()),
            "format" => Some("#{name}".to_owned()),
            "window_name" => Some("logs".to_owned()),
            _ => None,
        }
    }

    fn run(template: &str) -> String {
        expand(template, &vars)
    }

    #[test]
    fn expands_variables_and_short_forms() {
        assert_eq!(run("a #{name} b"), "a hello world b");
        assert_eq!(run("#W!"), "logs!");
        assert_eq!(run("[#{missing}]"), "[]");
        assert_eq!(run("#{format}"), "#{name}");
    }

    #[test]
    fn keeps_what_is_not_a_format() {
        assert_eq!(run(""), "");
        assert_eq!(run("100%"), "100%");
        assert_eq!(run("a#"), "a#");
        assert_eq!(run("#Q#\u{4e16}"), "#Q#\u{4e16}");
        assert_eq!(run("#{name"), "#{name");
    }

    #[test]
    fn unescapes() {
        assert_eq!(run("##W"), "#W");
        assert_eq!(run("#,#}"), ",}");
    }

    #[test]
    fn chooses_on_a_condition() {
        assert_eq!(run("#{?yes,a,b}"), "a");
        assert_eq!(run("#{?no,a,b}"), "b");
        assert_eq!(run("#{?empty,a,b}"), "b");
        assert_eq!(run("#{?missing,a,b}"), "b");
        assert_eq!(run("#{?no,a}"), "");
        assert_eq!(run("#{?no,a,yes,b,c}"), "b");
        assert_eq!(run("#{?no,a,no,b,c}"), "c");
        assert_eq!(run("#{?#{==:1,1},#{name},b}"), "hello world");
        assert_eq!(run("#{?yes,a#,b,c}"), "a,b");
    }

    #[test]
    fn nests_inside_conditions() {
        assert_eq!(run("#{?yes,#{?no,a,b},c}"), "b");
        assert_eq!(run("x#{?yes,#{yes}}y"), "x1y");
    }

    #[test]
    fn compares() {
        assert_eq!(run("#{==:a,a}"), "1");
        assert_eq!(run("#{==:#{yes},0}"), "0");
        assert_eq!(run("#{!=:a,b}"), "1");
        assert_eq!(run("#{<:a,b}"), "1");
        assert_eq!(run("#{>=:a,b}"), "0");
        assert_eq!(run("#{||:#{no},#{yes}}"), "1");
        assert_eq!(run("#{&&:#{no},#{yes}}"), "0");
        assert_eq!(run("#{!:no}"), "1");
        assert_eq!(run("#{!:#{yes}}"), "0");
    }

    #[test]
    fn truncates() {
        assert_eq!(run("#{=5:name}"), "hello");
        assert_eq!(run("#{=-5:name}"), "world");
        assert_eq!(run("#{=/5/...:name}"), "hello...");
        assert_eq!(run("#{=-/5/...:name}"), "");
        assert_eq!(run("#{=99:name}"), "hello world");
        assert_eq!(run("#{=3:wide}"), "\u{4e16}");
    }

    #[test]
    fn modifies_values() {
        assert_eq!(run("#{b:path}"), "bin");
        assert_eq!(run("#{d:path}"), "/usr/local");
        assert_eq!(run("#{l:#{name}}"), "#{name}");
        assert_eq!(run("#{n:name}"), "11");
        assert_eq!(run("#{w:wide}"), "5");
        assert_eq!(run("#{E:format}"), "hello world");
        assert_eq!(run("#{s/local/remote/:path}"), "/usr/remote/bin");
        assert_eq!(run("#{s|/|-|:path}"), "-usr-local-bin");
        assert_eq!(run("#{s/o/0/;=5:name}"), "hell0");
        assert_eq!(run("#{b:#{path}}"), "bin");
    }

    #[test]
    fn a_name_that_only_looks_like_a_modifier_is_a_variable() {
        assert_eq!(run("#{bad}"), "");
        assert_eq!(run("#{window_name}"), "logs");
    }
}
