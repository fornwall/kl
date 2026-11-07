//! The key table, and the names keys go by in it.

use crate::ghostty::key::{self, MOD_ALT, MOD_CTRL, MOD_SHIFT};
use crate::input::KeyEvent;

/// What a key does after the prefix.
pub struct Binding {
    pub key: &'static str,
    /// Keeps working without the prefix for a moment, as with tmux's `bind -r`.
    pub repeat: bool,
    pub command: &'static str,
    /// What `list-keys -N` says about it.
    pub note: &'static str,
}

const fn bind(key: &'static str, command: &'static str, note: &'static str) -> Binding {
    Binding {
        key,
        repeat: false,
        command,
        note,
    }
}

const fn repeat(key: &'static str, command: &'static str, note: &'static str) -> Binding {
    Binding {
        key,
        repeat: true,
        command,
        note,
    }
}

/// tmux's default key table, or the part of it that exists here, in its order.
pub const PREFIX_TABLE: &[Binding] = &[
    bind("\"", "split-window", "Split window vertically"),
    bind("#", "list-buffers", "List all paste buffers"),
    bind("%", "split-window -h", "Split window horizontally"),
    bind(
        "&",
        "confirm-before -p \"kill-window #W? (y/n)\" kill-window",
        "Kill current window",
    ),
    bind(
        ",",
        "command-prompt -I \"#W\" \"rename-window \\\"%%\\\"\"",
        "Rename current window",
    ),
    bind("-", "delete-buffer", "Delete the most recent paste buffer"),
    bind("0", "select-window -t :=0", "Select window 0"),
    bind("1", "select-window -t :=1", "Select window 1"),
    bind("2", "select-window -t :=2", "Select window 2"),
    bind("3", "select-window -t :=3", "Select window 3"),
    bind("4", "select-window -t :=4", "Select window 4"),
    bind("5", "select-window -t :=5", "Select window 5"),
    bind("6", "select-window -t :=6", "Select window 6"),
    bind("7", "select-window -t :=7", "Select window 7"),
    bind("8", "select-window -t :=8", "Select window 8"),
    bind("9", "select-window -t :=9", "Select window 9"),
    bind(":", "command-prompt", "Prompt for a command"),
    bind(";", "last-pane", "Move to the previously active pane"),
    bind("=", "choose-buffer -Z", "Choose a paste buffer from a list"),
    bind("?", "list-keys -N", "List key bindings"),
    bind("[", "copy-mode", "Enter copy mode"),
    bind("]", "paste-buffer -p", "Paste the most recent paste buffer"),
    bind("c", "new-window", "Create a new window"),
    bind("d", "detach-client", "Detach the current client"),
    bind("i", "display-message", "Display window information"),
    bind("l", "last-window", "Select the previously current window"),
    bind("n", "next-window", "Select the next window"),
    bind("o", "select-pane -t :.+", "Select the next pane"),
    bind("p", "previous-window", "Select the previous window"),
    bind("q", "display-panes", "Display pane numbers"),
    bind("w", "choose-tree -Zw", "Choose a window from a list"),
    bind(
        "x",
        "confirm-before -p \"kill-pane #P? (y/n)\" kill-pane",
        "Kill the active pane",
    ),
    bind("z", "resize-pane -Z", "Zoom the active pane"),
    bind("PPage", "copy-mode -u", "Enter copy mode and scroll up"),
    repeat(
        "Up",
        "select-pane -U",
        "Select the pane above the active pane",
    ),
    repeat(
        "Down",
        "select-pane -D",
        "Select the pane below the active pane",
    ),
    repeat(
        "Left",
        "select-pane -L",
        "Select the pane to the left of the active pane",
    ),
    repeat(
        "Right",
        "select-pane -R",
        "Select the pane to the right of the active pane",
    ),
    bind(
        "M-n",
        "next-window -a",
        "Select the next window with an alert",
    ),
    bind(
        "M-p",
        "previous-window -a",
        "Select the previous window with an alert",
    ),
    repeat("M-Up", "resize-pane -U 5", "Resize the pane up by 5"),
    repeat("M-Down", "resize-pane -D 5", "Resize the pane down by 5"),
    repeat("M-Left", "resize-pane -L 5", "Resize the pane left by 5"),
    repeat("M-Right", "resize-pane -R 5", "Resize the pane right by 5"),
    bind("C-b", "send-prefix", "Send the prefix key"),
    repeat("C-Up", "resize-pane -U", "Resize the pane up"),
    repeat("C-Down", "resize-pane -D", "Resize the pane down"),
    repeat("C-Left", "resize-pane -L", "Resize the pane left"),
    repeat("C-Right", "resize-pane -R", "Resize the pane right"),
];

/// A key's name in tmux's notation, such as `C-Up`, `M-n` or `%`.
fn key_name(event: &KeyEvent) -> Option<String> {
    let mods = event.binding_mods();
    let special = match event.key {
        key::ARROW_UP => Some("Up"),
        key::ARROW_DOWN => Some("Down"),
        key::ARROW_LEFT => Some("Left"),
        key::ARROW_RIGHT => Some("Right"),
        key::PAGE_UP => Some("PPage"),
        key::PAGE_DOWN => Some("NPage"),
        key::SPACE => Some("Space"),
        _ => None,
    };
    let (name, mods) = match special {
        Some(name) => (name.to_owned(), mods),
        // Shift already went into making the character.
        None => (event.char()?.to_string(), mods & !MOD_SHIFT),
    };
    let prefix = match mods {
        0 => "",
        MOD_CTRL => "C-",
        MOD_ALT => "M-",
        MOD_SHIFT => "S-",
        _ => return None,
    };
    Some(format!("{prefix}{name}"))
}

/// What `list-keys` prints, or `list-keys -N` to explain the keys in words.
pub fn list(notes: bool) -> Vec<String> {
    let width = PREFIX_TABLE.iter().map(|binding| binding.key.len()).max();
    let width = width.unwrap_or(0);
    let line = |binding: &Binding| {
        let Binding {
            key,
            repeat,
            command,
            note,
        } = binding;
        if notes {
            format!("C-b {key:width$} {note}")
        } else {
            let repeat = if *repeat { "-r" } else { "  " };
            format!("bind-key {repeat} -T prefix {key:width$} {command}")
        }
    };
    PREFIX_TABLE.iter().map(line).collect()
}

/// The binding of a key pressed after the prefix.
pub fn binding(event: &KeyEvent) -> Option<&'static Binding> {
    let name = key_name(event)?;
    PREFIX_TABLE.iter().find(|binding| binding.key == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command;
    use crate::input::{Event, Parser};

    fn name(bytes: &[u8]) -> Option<String> {
        let events = Parser::default().feed(bytes);
        events.iter().find_map(|event| match event {
            Event::Key(event) => key_name(event),
            _ => None,
        })
    }

    #[test]
    fn every_binding_is_a_command() {
        for binding in PREFIX_TABLE {
            let parsed = command::parse_line(binding.command);
            assert!(parsed.is_ok(), "{}: {parsed:?}", binding.command);
        }
    }

    #[test]
    fn lists_keys_like_tmux() {
        let lines = list(false);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("bind-key    -T prefix %       split-window -h"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("bind-key -r -T prefix Up      select-pane -U"))
        );
        let lines = list(true);
        assert!(lines.contains(&"C-b z       Zoom the active pane".to_owned()));
    }

    #[test]
    fn keys_are_bound_once() {
        for (index, binding) in PREFIX_TABLE.iter().enumerate() {
            let others = PREFIX_TABLE.iter().skip(index + 1);
            assert!(
                others.into_iter().all(|other| other.key != binding.key),
                "{}",
                binding.key
            );
        }
    }

    #[test]
    fn names_keys_as_tmux_does() {
        assert_eq!(name(b"c").as_deref(), Some("c"));
        assert_eq!(name(b"%").as_deref(), Some("%"));
        assert_eq!(name(b" ").as_deref(), Some("Space"));
        assert_eq!(name(&[2]).as_deref(), Some("C-b"));
        assert_eq!(name(b"\x1bn").as_deref(), Some("M-n"));
        assert_eq!(name(b"\x1b[A").as_deref(), Some("Up"));
        assert_eq!(name(b"\x1b[1;5A").as_deref(), Some("C-Up"));
        assert_eq!(name(b"\x1b[1;3D").as_deref(), Some("M-Left"));
        assert_eq!(name(b"\x1b[1;2C").as_deref(), Some("S-Right"));
        assert_eq!(name(b"\x1b[5~").as_deref(), Some("PPage"));
    }

    #[test]
    fn shift_is_part_of_a_character() {
        // The kitty keyboard protocol reports both the shift and the text.
        assert_eq!(name(b"\x1b[53;2;37u").as_deref(), Some("%"));
    }
}
