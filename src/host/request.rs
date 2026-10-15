//! What a session asks mothership to run: the request files in its `runs/` directory and
//! their answers next to them.

use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Codex's `model_reasoning_effort` levels.
const EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

/// What a request asks for.
#[derive(Debug, PartialEq)]
pub enum Request {
    Codex(CodexRun),
    /// One of the repository's `commands`, by name.
    Command(String),
}

/// A Codex run: the prompt it gets on stdin, and what shapes its final message.
#[derive(Debug, PartialEq)]
pub struct CodexRun {
    pub prompt: String,
    /// A JSON schema the final message must match (`--output-schema`); its answer is that
    /// JSON rather than the message text.
    pub schema: Option<Value>,
    pub effort: Option<String>,
    pub model: Option<String>,
}

/// `{"kind": "codex", "prompt": …, "schema"?: {…}, "effort"?: …, "model"?: …}` or
/// `{"kind": "command", "name": …}`.
pub fn parse(text: &str) -> Result<Request, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    match value["kind"].as_str() {
        Some("codex") => codex(&value).map(Request::Codex),
        Some("command") => match value["name"].as_str() {
            Some(name) if plain(name) => Ok(Request::Command(name.to_string())),
            _ => Err("`name` must name one of the repository's commands".into()),
        },
        _ => Err("`kind` must be \"codex\" or \"command\"".into()),
    }
}

fn codex(value: &Value) -> Result<CodexRun, String> {
    let prompt = value["prompt"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
        .ok_or("`prompt` must be a non-empty string")?;
    let schema = match &value["schema"] {
        Value::Null => None,
        schema @ Value::Object(_) => Some(schema.clone()),
        _ => return Err("`schema` must be a JSON object".into()),
    };
    let text_field = |name: &str| match &value[name] {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(format!("`{name}` must be a string")),
    };
    let effort = text_field("effort")?;
    if let Some(effort) = effort.as_deref().filter(|e| !EFFORTS.contains(e)) {
        return Err(format!("`effort` {effort:?} is not one of {EFFORTS:?}"));
    }
    let model = text_field("model")?;
    if let Some(model) = model.as_deref().filter(|m| !plain(m) || m.starts_with('-')) {
        return Err(format!("`model` {model:?} is not a model name"));
    }
    Ok(CodexRun {
        prompt: prompt.to_string(),
        schema,
        effort,
        model,
    })
}

/// The ids of requests in `dir` that have no answer yet, oldest name first.
pub fn pending(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(String::from)
        })
        .filter(|id| plain(id) && !id.contains('.') && !answer(dir, id).exists())
        .collect();
    ids.sort();
    ids
}

pub fn request(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn answer(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.out.json"))
}

/// Writes the answer whole, since the agent polls for it: created fresh under a name nobody
/// can predict, then renamed into place, which replaces whatever is at the answer's name, a
/// symlink too, without following it.
pub fn write_answer(dir: &Path, id: &str, outcome: Result<Value, String>) -> io::Result<()> {
    let body = match outcome {
        Ok(result) => json!({ "result": result }),
        Err(error) => json!({ "error": error }),
    };
    let tmp = dir.join(format!(".{id}.{}.answer", unique()));
    write_new(&tmp, body.to_string().as_bytes())?;
    std::fs::rename(tmp, answer(dir, id))
}

/// A name part no other process can guess ahead of time.
pub fn unique() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos:x}", std::process::id())
}

// The agent can place anything in its `runs/` directory, and a run's commands anything in the
// run's own directory, while mothership runs unconfined. So a file there is only read when it
// is a plain file with no other name, and only written fresh: a symlink or a hard link planted
// there could otherwise point mothership at a file the session may not read or write.

/// Reads `path` when it is a plain file with a single link and at most a mebibyte: opened
/// without following a link, and without waiting on a FIFO, then checked as opened.
pub fn read_plain(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    const CAP: u64 = 1 << 20;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() || meta.nlink() != 1 {
        return Err(io::Error::other("not a plain file"));
    }
    if meta.len() > CAP {
        return Err(io::Error::other("larger than a mebibyte"));
    }
    let mut text = String::new();
    file.take(CAP).read_to_string(&mut text)?;
    Ok(text)
}

/// Creates `path` anew with `bytes`, removing what was there first; creation fails rather than
/// follow a link put back in between.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    remove(path)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(bytes)
}

/// Removes `path` (a link itself, not its target) if it is there.
pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Letters, digits and `._:-` only, at most 64: safe as a file name part and an argument.
fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_codex_request() {
        let Ok(Request::Codex(r)) = parse(
            r#"{"kind":"codex","prompt":"review","schema":{"type":"object"},"effort":"high","model":"gpt-5.5"}"#,
        ) else {
            panic!("a codex request");
        };
        assert_eq!(r.prompt, "review");
        assert_eq!(r.schema, Some(json!({"type": "object"})));
        assert_eq!(
            (r.effort.as_deref(), r.model.as_deref()),
            (Some("high"), Some("gpt-5.5"))
        );
        let Ok(Request::Codex(bare)) = parse(r#"{"kind":"codex","prompt":"p"}"#) else {
            panic!("a codex request");
        };
        assert_eq!((bare.schema, bare.effort, bare.model), (None, None, None));
        assert_eq!(
            parse(r#"{"kind":"command","name":"pnpm-install"}"#),
            Ok(Request::Command("pnpm-install".into()))
        );
    }

    #[test]
    fn refuses_what_it_cannot_run_safely() {
        for bad in [
            "nope",
            r#"{"kind":"shell","prompt":"ls"}"#,
            r#"{"kind":"codex","prompt":" "}"#,
            r#"{"kind":"codex","prompt":"p","schema":"x"}"#,
            r#"{"kind":"codex","prompt":"p","effort":"max"}"#,
            r#"{"kind":"codex","prompt":"p","model":"gpt -c x=1"}"#,
            r#"{"kind":"codex","prompt":"p","model":7}"#,
            r#"{"kind":"codex","prompt":"p","model":"--config"}"#,
            r#"{"kind":"command","name":"rm -rf /"}"#,
            r#"{"kind":"command"}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lists_unanswered_requests_only() {
        let dir = std::env::temp_dir().join(format!("mothership-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "b.json",
            "a.json",
            "a.out.json",
            "c.json",
            "c.out.tmp",
            "x.schema.json",
            "bad id.json",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        assert_eq!(pending(&dir), vec!["b", "c"]);
        write_answer(&dir, "b", Ok(json!({"findings": []}))).unwrap();
        write_answer(&dir, "c", Err("no".into())).unwrap();
        assert_eq!(pending(&dir), Vec::<String>::new());
        let read = |id| std::fs::read_to_string(answer(&dir, id)).unwrap();
        assert_eq!(read("b"), r#"{"result":{"findings":[]}}"#);
        assert_eq!(read("c"), r#"{"error":"no"}"#);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn neither_follows_nor_reads_planted_links() {
        let dir = std::env::temp_dir().join(format!("mothership-links-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret");
        std::fs::write(&secret, "s3cret").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("sym.json")).unwrap();
        std::fs::hard_link(&secret, dir.join("hard.json")).unwrap();
        assert!(read_plain(&dir.join("sym.json")).is_err());
        assert!(read_plain(&dir.join("hard.json")).is_err());
        let fifo = dir.join("fifo.json");
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(
            read_plain(&fifo).is_err(),
            "a FIFO is refused without blocking"
        );
        std::fs::write(dir.join("big.json"), vec![b' '; (1 << 20) + 1]).unwrap();
        assert!(read_plain(&dir.join("big.json")).is_err());
        std::fs::write(dir.join("ok.json"), "{}").unwrap();
        assert_eq!(read_plain(&dir.join("ok.json")).unwrap(), "{}");

        std::os::unix::fs::symlink(&secret, dir.join("a.out.json")).unwrap();
        write_answer(&dir, "a", Err("no".into())).unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "s3cret");
        assert!(
            !std::fs::symlink_metadata(answer(&dir, "a"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
