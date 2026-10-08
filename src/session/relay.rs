//! What a running turn shows: agent events turned into surface updates, and lines tools
//! append to the progress file.

use super::Update;
use crate::agent::Event;
use anyhow::Result;
use std::path::PathBuf;

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
            Event::Started { .. } | Event::Result { .. } => {}
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
pub(super) struct ProgressTail {
    pub(super) path: PathBuf,
    offset: usize,
}

impl ProgressTail {
    pub(super) fn create(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, "")?;
        Ok(Self { path, offset: 0 })
    }

    pub(super) fn read_new(&mut self) -> Vec<String> {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return Vec::new();
        };
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            return Vec::new();
        };
        if end <= self.offset {
            return Vec::new();
        }
        let lines = String::from_utf8_lossy(&bytes[self.offset..end])
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        self.offset = end;
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        std::fs::remove_file(&path).unwrap();
    }
}
