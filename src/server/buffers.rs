//! Paste buffers, as tmux keeps them. Copying makes a new buffer with a name of
//! its own, of which the oldest go once there are too many. Those the user gives
//! a name stay until they get deleted.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::Server;
use super::ui::{List, ListRow};
use crate::command::{BufferCommand, Command, PasteOptions, SetBuffer};
use crate::ghostty::{MODE_BRACKETED_PASTE, Terminal};

/// tmux's `buffer-limit`.
const BUFFER_LIMIT: usize = 50;

/// How much of a buffer `list-buffers` shows.
const SAMPLE_BYTES: usize = 200;

/// A control character in the way of C.
fn octal(c: char) -> [char; 4] {
    let digit = |shift: u32| char::from_digit((u32::from(c) >> shift) & 7, 8).unwrap_or('0');
    ['\\', digit(6), digit(3), digit(0)]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub name: String,
    pub text: String,
    /// It got its name here, rather than from the user.
    pub automatic: bool,
}

impl Buffer {
    /// The text in a line of its own, with what is special made visible.
    fn sample(&self) -> String {
        let mut sample = String::new();
        let shown = self.text.char_indices();
        for (_, c) in shown.take_while(|(at, _)| *at < SAMPLE_BYTES) {
            match c {
                '\n' => sample.push_str("\\n"),
                '\r' => sample.push_str("\\r"),
                '\t' => sample.push_str("\\t"),
                '\\' => sample.push_str("\\\\"),
                c if c.is_control() => sample.extend(octal(c)),
                c => sample.push(c),
            }
        }
        if self.text.len() > SAMPLE_BYTES {
            sample.push_str("...");
        }
        sample
    }

    /// As `list-buffers` shows it.
    fn describe(&self) -> String {
        let (name, size, sample) = (&self.name, self.text.len(), self.sample());
        format!("{name}: {size} bytes: \"{sample}\"")
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Buffers {
    /// The newest first.
    pub list: Vec<Buffer>,
    /// What goes in the name of the next automatic buffer.
    pub next_index: u32,
}

impl Buffers {
    /// Adds a buffer with a name of its own, dropping the oldest of these if
    /// there are too many.
    pub fn add(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let automatic = self.list.iter().filter(|buffer| buffer.automatic).count();
        if automatic >= BUFFER_LIMIT
            && let Some(oldest) = self.list.iter().rposition(|buffer| buffer.automatic)
        {
            self.list.remove(oldest);
        }
        let name = loop {
            let name = format!("buffer{}", self.next_index);
            self.next_index = self.next_index.wrapping_add(1);
            if self.named(&name).is_none() {
                break name;
            }
        };
        self.list.insert(
            0,
            Buffer {
                name,
                text,
                automatic: true,
            },
        );
    }

    /// Makes a buffer with a name, in place of any other of that name.
    fn set(&mut self, name: &str, text: String) -> Result<(), String> {
        if name.is_empty() {
            return Err("empty buffer name".to_owned());
        }
        self.delete(name);
        if !text.is_empty() {
            self.list.insert(
                0,
                Buffer {
                    name: name.to_owned(),
                    text,
                    automatic: false,
                },
            );
        }
        Ok(())
    }

    /// The buffer that is used when none is asked for.
    pub fn top(&self) -> Option<&Buffer> {
        self.list.iter().find(|buffer| buffer.automatic)
    }

    fn named(&self, name: &str) -> Option<&Buffer> {
        self.list.iter().find(|buffer| buffer.name == name)
    }

    fn delete(&mut self, name: &str) -> bool {
        let found = self.list.iter().position(|buffer| buffer.name == name);
        found.map(|index| self.list.remove(index)).is_some()
    }

    /// Adds text to the end of the top buffer, which becomes the newest.
    pub fn append_to_top(&mut self, text: &str) {
        match self.list.iter().position(|buffer| buffer.automatic) {
            Some(index) => {
                let mut buffer = self.list.remove(index);
                buffer.text.push_str(text);
                self.list.insert(0, buffer);
            }
            None => self.add(text.to_owned()),
        }
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        if new.is_empty() {
            return Err("new name is empty".to_owned());
        }
        if old != new {
            self.delete(new);
        }
        let found = self.list.iter_mut().find(|buffer| buffer.name == old);
        let buffer = found.ok_or_else(|| format!("no buffer {old}"))?;
        new.clone_into(&mut buffer.name);
        buffer.automatic = false;
        Ok(())
    }

    /// The buffer of that name, or the top one without a name. There is no such
    /// thing as an error for that last one, since not all commands mind.
    fn find(&self, name: Option<&str>) -> Result<Option<&Buffer>, String> {
        let Some(name) = name else {
            return Ok(self.top());
        };
        let found = self.named(name);
        found.map(Some).ok_or_else(|| format!("no buffer {name}"))
    }

    /// The name of the buffer to work on, as for [`Self::find`] but with tmux's
    /// other message for one that is not there.
    fn existing_name(&self, name: Option<&str>) -> Result<String, String> {
        match name {
            Some(name) if self.named(name).is_none() => Err(format!("unknown buffer: {name}")),
            Some(name) => Ok(name.to_owned()),
            None => self
                .top()
                .map(|buffer| buffer.name.clone())
                .ok_or_else(|| "no buffer".to_owned()),
        }
    }
}

/// What a pane gets for pasting some text.
fn paste_bytes(terminal: &Terminal, text: &str, options: &PasteOptions) -> Vec<u8> {
    let separator = options.separator.as_deref();
    let separator = separator.or_else(|| options.raw.then_some("\n"));
    let Some(separator) = separator else {
        return terminal.encode_paste(text.as_bytes(), options.bracket);
    };
    let lines: Vec<Vec<u8>> = text
        .split('\n')
        .map(|line| terminal.encode_paste(line.as_bytes(), false))
        .collect();
    let bytes = lines.join(separator.as_bytes());
    if options.bracket && terminal.mode(MODE_BRACKETED_PASTE) {
        [b"\x1b[200~", bytes.as_slice(), b"\x1b[201~"].concat()
    } else {
        bytes
    }
}

/// An error about a file, the way tmux puts it.
fn file_error(error: &io::Error, path: &Path) -> String {
    let text = error.to_string();
    let reason = text.split(" (os error").next().unwrap_or(&text);
    format!("{reason}: {}", path.display())
}

/// Stops a file that is not a plain one, such as a pipe without a writer, from
/// holding up every pane by blocking the server.
fn check_plain_file(path: &Path) -> Result<(), String> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => Err(format!("not a file: {}", path.display())),
        _ => Ok(()),
    }
}

fn save(path: &Path, text: &str, append: bool) -> Result<(), String> {
    check_plain_file(path)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path);
    file.and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| file_error(&error, path))
}

fn load(path: &Path) -> Result<String, String> {
    check_plain_file(path)?;
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| file_error(&error, path))
}

impl Server {
    /// A copy from a pane, which goes on the clipboard as well.
    pub fn store_buffer(&mut self, text: String) {
        if !text.is_empty() {
            self.send_clipboard(text.as_bytes());
            self.buffers.add(text);
        }
    }

    /// Adds a copy to the last one. Only what is new goes on the clipboard.
    pub fn append_buffer(&mut self, text: &str) {
        if !text.is_empty() {
            self.send_clipboard(text.as_bytes());
            self.buffers.append_to_top(text);
        }
    }

    pub fn run_buffer_command(&mut self, command: BufferCommand) -> Result<String, String> {
        match command {
            BufferCommand::Choose => self.choose_buffer(0),
            BufferCommand::Delete(name) => {
                let name = self.buffers.existing_name(name.as_deref())?;
                self.buffers.delete(&name);
            }
            BufferCommand::List => return Ok(self.list_buffers()),
            BufferCommand::Load {
                name,
                path,
                clipboard,
            } => self.load_buffer(name.as_deref(), &path, clipboard)?,
            BufferCommand::Paste(options) => self.paste_buffer(&options)?,
            BufferCommand::Save { name, path, append } => {
                return self.save_buffer(name.as_deref(), &path, append);
            }
            BufferCommand::Set(set) => self.set_buffer(set)?,
            BufferCommand::Show(name) => return self.text_of(name.as_deref()),
        }
        Ok(String::new())
    }

    fn list_buffers(&self) -> String {
        let lines: Vec<String> = self.buffers.list.iter().map(Buffer::describe).collect();
        lines.iter().flat_map(|line| [line, "\n"]).collect()
    }

    /// Lets the user pick a buffer to paste from a list, on the row given.
    pub fn choose_buffer(&mut self, selected: usize) {
        let row = |(index, buffer): (usize, &Buffer)| {
            let paste = Command::Buffer(BufferCommand::Paste(PasteOptions {
                name: Some(buffer.name.clone()),
                bracket: true,
                ..PasteOptions::default()
            }));
            let delete = Command::Buffer(BufferCommand::Delete(Some(buffer.name.clone())));
            ListRow::new(format!("({index}) {}", buffer.describe()), paste).with_delete(delete)
        };
        let rows: Vec<ListRow> = self.buffers.list.iter().enumerate().map(row).collect();
        if rows.is_empty() {
            self.ui.list = None;
            self.session.repaint = true;
        } else {
            let selected = selected.min(rows.len() - 1);
            self.ui.list = Some(List::new(rows, selected));
        }
    }

    fn paste_buffer(&mut self, options: &PasteOptions) -> Result<(), String> {
        let Some(buffer) = self.buffers.find(options.name.as_deref())? else {
            return Ok(());
        };
        let name = buffer.name.clone();
        let id = self.session.find_pane(options.target.as_deref())?;
        if let Some(pane) = self.session.panes.get_mut(&id) {
            let bytes = paste_bytes(&pane.terminal, &buffer.text, options);
            pane.send(&bytes);
        }
        if options.delete {
            self.buffers.delete(&name);
        }
        Ok(())
    }

    fn text_of(&self, name: Option<&str>) -> Result<String, String> {
        let buffer = self.buffers.find(name)?.ok_or("no buffers")?;
        Ok(buffer.text.clone())
    }

    /// Where a path goes, which for a relative one is the directory of the
    /// active pane. The server has no directory of its own that means anything.
    fn resolve(&self, path: PathBuf) -> PathBuf {
        let pane = self.session.active_pane();
        let directory = pane.and_then(super::pane::Pane::working_directory);
        match directory {
            Some(directory) if path.is_relative() => directory.join(path),
            _ => path,
        }
    }

    fn save_buffer(&self, name: Option<&str>, path: &Path, append: bool) -> Result<String, String> {
        let text = self.text_of(name)?;
        if path == Path::new("-") {
            return Ok(text);
        }
        save(&self.resolve(path.to_owned()), &text, append)?;
        Ok(String::new())
    }

    fn load_buffer(
        &mut self,
        name: Option<&str>,
        path: &Path,
        clipboard: bool,
    ) -> Result<(), String> {
        let text = load(&self.resolve(path.to_owned()))?;
        self.fill_buffer(name, text, clipboard)
    }

    fn fill_buffer(
        &mut self,
        name: Option<&str>,
        text: String,
        clipboard: bool,
    ) -> Result<(), String> {
        if clipboard {
            self.send_clipboard(text.as_bytes());
        }
        if let Some(name) = name {
            return self.buffers.set(name, text);
        }
        self.buffers.add(text);
        Ok(())
    }

    fn set_buffer(&mut self, set: SetBuffer) -> Result<(), String> {
        if let Some(new_name) = &set.new_name {
            let name = self.buffers.existing_name(set.name.as_deref())?;
            return self.buffers.rename(&name, new_name);
        }
        let data = set.data.ok_or("no data specified")?;
        if data.is_empty() {
            return Ok(());
        }
        let name = set.name.as_deref();
        let old = name.and_then(|name| self.buffers.named(name));
        let text = match old {
            Some(old) if set.append => format!("{}{data}", old.text),
            _ => data,
        };
        self.fill_buffer(name, text, set.clipboard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(buffers: &Buffers) -> Vec<&str> {
        let names = buffers.list.iter().map(|buffer| buffer.name.as_str());
        names.collect()
    }

    fn with_text(text: &str) -> Buffer {
        Buffer {
            name: "x".to_owned(),
            text: text.to_owned(),
            automatic: true,
        }
    }

    #[test]
    fn numbers_copies_with_the_newest_first() {
        let mut buffers = Buffers::default();
        buffers.add("one".to_owned());
        buffers.add("two".to_owned());
        assert_eq!(names(&buffers), ["buffer1", "buffer0"]);
        assert_eq!(buffers.top().map(|top| top.text.as_str()), Some("two"));
    }

    #[test]
    fn leaves_nothing_for_empty_text() {
        let mut buffers = Buffers::default();
        buffers.add(String::new());
        assert_eq!(names(&buffers), [""; 0]);
    }

    #[test]
    fn drops_the_oldest_copies_but_not_named_buffers() {
        let mut buffers = Buffers::default();
        buffers.set("kept", "text".to_owned()).unwrap();
        for index in 0..=BUFFER_LIMIT {
            buffers.add(index.to_string());
        }
        assert_eq!(buffers.list.len(), BUFFER_LIMIT + 1);
        assert!(buffers.named("kept").is_some());
        assert!(buffers.named("buffer0").is_none());
        assert!(buffers.named("buffer1").is_some());
    }

    #[test]
    fn the_top_buffer_is_not_one_with_a_given_name() {
        let mut buffers = Buffers::default();
        buffers.set("named", "text".to_owned()).unwrap();
        assert_eq!(buffers.top(), None);
        buffers.add("copy".to_owned());
        buffers.set("newer", "text".to_owned()).unwrap();
        assert_eq!(buffers.top().map(|top| top.text.as_str()), Some("copy"));
    }

    #[test]
    fn setting_a_name_again_replaces_it_and_makes_it_the_newest() {
        let mut buffers = Buffers::default();
        buffers.set("a", "old".to_owned()).unwrap();
        buffers.set("b", "text".to_owned()).unwrap();
        buffers.set("a", "new".to_owned()).unwrap();
        assert_eq!(names(&buffers), ["a", "b"]);
        assert_eq!(buffers.named("a").map(|a| a.text.as_str()), Some("new"));
        assert!(buffers.set("", "text".to_owned()).is_err());
    }

    #[test]
    fn renaming_takes_a_buffer_out_of_the_automatic_ones() {
        let mut buffers = Buffers::default();
        buffers.add("copy".to_owned());
        buffers.set("taken", "other".to_owned()).unwrap();
        buffers.rename("buffer0", "taken").unwrap();
        assert_eq!(names(&buffers), ["taken"]);
        assert_eq!(buffers.top(), None);
        assert!(buffers.rename("gone", "new").is_err());
        assert!(buffers.rename("taken", "").is_err());
        buffers.rename("taken", "taken").unwrap();
        assert_eq!(names(&buffers), ["taken"]);
    }

    #[test]
    fn appends_to_the_top_buffer_or_starts_one() {
        let mut buffers = Buffers::default();
        buffers.append_to_top("one");
        buffers.append_to_top(" two");
        assert_eq!(buffers.top().map(|top| top.text.as_str()), Some("one two"));
        assert_eq!(names(&buffers), ["buffer0"]);
    }

    #[test]
    fn says_what_is_missing_the_way_tmux_does() {
        let buffers = Buffers::default();
        assert_eq!(buffers.existing_name(None).unwrap_err(), "no buffer");
        assert_eq!(
            buffers.existing_name(Some("x")).unwrap_err(),
            "unknown buffer: x"
        );
        assert_eq!(buffers.find(Some("x")).unwrap_err(), "no buffer x");
        assert_eq!(buffers.find(None), Ok(None));
    }

    #[test]
    fn samples_show_what_is_special() {
        let sample = with_text("a\nb\tc\\d\x1be").sample();
        assert_eq!(sample, "a\\nb\\tc\\\\d\\033e");
        let long = "x".repeat(SAMPLE_BYTES + 1);
        let expected = format!("{}...", "x".repeat(SAMPLE_BYTES));
        assert_eq!(with_text(&long).sample(), expected);
    }

    #[test]
    fn a_sample_does_not_split_a_character() {
        let text = "\u{4e16}".repeat(SAMPLE_BYTES);
        assert!(with_text(&text).sample().ends_with("\u{4e16}..."));
    }

    #[test]
    fn describes_a_buffer() {
        let buffer = Buffer {
            name: "buffer3".to_owned(),
            text: "hi\n".to_owned(),
            automatic: true,
        };
        assert_eq!(buffer.describe(), "buffer3: 3 bytes: \"hi\\n\"");
    }

    #[test]
    fn separators_stand_in_for_line_breaks() {
        let terminal = Terminal::new(10, 2).unwrap();
        let paste = |options| paste_bytes(&terminal, "a\nb\n", &options);
        assert_eq!(paste(PasteOptions::default()), b"a\rb\r");
        let raw = PasteOptions {
            raw: true,
            ..PasteOptions::default()
        };
        assert_eq!(paste(raw), b"a\nb\n");
        let separated = PasteOptions {
            separator: Some("--".to_owned()),
            ..PasteOptions::default()
        };
        assert_eq!(paste(separated), b"a--b--");
    }

    #[test]
    fn brackets_only_when_asked_and_when_the_program_wants_them() {
        let mut terminal = Terminal::new(10, 2).unwrap();
        terminal.write(b"\x1b[?2004h");
        let plain = PasteOptions::default();
        assert_eq!(paste_bytes(&terminal, "ab", &plain), b"ab");
        let bracket = PasteOptions {
            bracket: true,
            ..PasteOptions::default()
        };
        let bracketed = b"\x1b[200~ab\x1b[201~";
        assert_eq!(paste_bytes(&terminal, "ab", &bracket), bracketed);
        let both = PasteOptions {
            raw: true,
            ..bracket
        };
        assert_eq!(paste_bytes(&terminal, "ab", &both), bracketed);
    }

    #[test]
    fn saves_and_loads_files() {
        let dir = std::env::temp_dir().join(format!("kl-buffers-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("saved");
        save(&path, "one", false).unwrap();
        save(&path, " two", true).unwrap();
        assert_eq!(load(&path).unwrap(), "one two");
        save(&path, "new", false).unwrap();
        assert_eq!(load(&path).unwrap(), "new");
        assert!(load(&dir).is_err(), "a directory is not a file");
        assert!(save(&dir, "text", false).is_err());
        let missing = dir.join("missing");
        let error = load(&missing).unwrap_err();
        assert!(error.ends_with("missing"), "{error}");
        assert!(!error.contains("os error"), "{error}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
