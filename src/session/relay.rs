//! What a running turn shows: agent events turned into surface updates, and lines tools
//! append to the progress file.

use super::Update;
use crate::agent::Event;
use anyhow::Result;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::PathBuf,
};

/// Turns agent events into surface updates. The newest text is held back because the
/// final one comes again as the result.
#[derive(Default)]
pub(super) struct Relay {
    pending: Option<(String, bool)>,
}

impl Relay {
    pub(super) fn on(&mut self, event: Event) -> Vec<Update> {
        let mut out = Vec::new();
        match event {
            Event::Text { text, nested } => {
                out.extend(self.take());
                self.pending = Some((text, nested));
            }
            Event::Tool {
                name,
                input,
                nested,
            } => {
                out.extend(self.take());
                out.push(Update::Tool {
                    name,
                    input,
                    nested,
                });
            }
            Event::Started { .. }
            | Event::Result { .. }
            | Event::Usage(_)
            | Event::Background { .. }
            | Event::Limited { .. } => {}
        }
        out
    }

    /// Releases held text unless it is the result about to be posted.
    pub(super) fn flush_except(&mut self, result: &str) -> Vec<Update> {
        match self.pending.take() {
            Some((text, _)) if text == result => Vec::new(),
            Some((text, nested)) => vec![Update::Thought { text, nested }],
            None => Vec::new(),
        }
    }

    fn take(&mut self) -> Option<Update> {
        self.pending
            .take()
            .map(|(text, nested)| Update::Thought { text, nested })
    }
}

/// New complete lines of a progress file that tools in the agent's process tree append to.
/// The file is created fresh and read through the handle opened then: the agent may write
/// its directory, and a link put in the file's place must not make mothership read or write
/// something else.
pub(super) struct ProgressTail {
    pub(super) path: PathBuf,
    file: File,
    offset: u64,
}

impl ProgressTail {
    pub(super) fn create(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            offset: 0,
        })
    }

    /// Another handle on the same file, appending: for what mothership reports itself.
    pub(super) fn handle(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }

    pub(super) fn read_new(&mut self) -> Vec<String> {
        /// At most this much is read per call; anything older that a burst (or a file grown
        /// sparse to gigabytes) pushed past it is skipped rather than held in memory.
        const WINDOW: u64 = 1 << 20;
        let Ok(len) = self.file.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        self.offset = self.offset.max(len.saturating_sub(WINDOW));
        let Ok(size) = usize::try_from(len.saturating_sub(self.offset)) else {
            return Vec::new();
        };
        let mut bytes = vec![0; size];
        // A positioned read: appends through the other handle move the shared offset.
        if size == 0 || self.file.read_exact_at(&mut bytes, self.offset).is_err() {
            return Vec::new();
        }
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            return Vec::new();
        };
        let lines = String::from_utf8_lossy(&bytes[..end])
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        self.offset += end as u64;
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn relay_holds_back_the_final_text() {
        let mut relay = Relay::default();
        let mut out = Vec::new();
        out.extend(relay.on(Event::Text {
            text: "Looking.".into(),
            nested: false,
        }));
        out.extend(relay.on(Event::Tool {
            name: "Bash".into(),
            input: json!({}),
            nested: true,
        }));
        out.extend(relay.on(Event::Text {
            text: "Done.".into(),
            nested: false,
        }));
        out.extend(relay.flush_except("Done."));
        assert_eq!(
            out,
            vec![
                Update::Thought {
                    text: "Looking.".into(),
                    nested: false
                },
                Update::Tool {
                    name: "Bash".into(),
                    input: json!({}),
                    nested: true
                },
            ]
        );
    }

    #[test]
    fn progress_tail_returns_only_complete_new_lines() {
        let path = std::env::temp_dir().join(format!("mothership-progress-{}", std::process::id()));
        let mut tail = ProgressTail::create(path.clone()).unwrap();
        std::fs::write(&path, "phase: build\nhalf").unwrap();
        assert_eq!(tail.read_new(), vec!["phase: build"]);
        std::fs::write(&path, "phase: build\nhalf done\n").unwrap();
        assert_eq!(tail.read_new(), vec!["half done"]);
        assert_eq!(tail.read_new(), Vec::<String>::new());
        // A file grown sparse to gigabytes is skipped to its last window, not read whole.
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.set_len(1 << 34).unwrap();
        writeln!(&file, "\nlast").unwrap();
        assert_eq!(tail.read_new().last().map(String::as_str), Some("last"));
        std::fs::remove_file(&path).unwrap();
    }
}
