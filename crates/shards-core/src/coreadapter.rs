// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! What the core's session runner needs to know about the agent tool, kept
//! behind [`CoreAdapter`] so nothing else assumes Claude Code.
//!
//! An adapter says how to start the core (fresh, with a first message, or
//! resuming a pinned session by its id), how to tell it to exit, how to find
//! the id of a session it just started, and where that session's transcript
//! is. [`TranscriptScan`] reads a transcript incrementally for the few things
//! the core runner needs: the newest entry time, when the last prompt
//! arrived, and the token usage of each reply. Like the shard activity
//! reader, it keeps times and numbers only. Prompt text is looked at for one
//! thing, whether a prompt is exactly a `[HH:MM]` heartbeat, and is never
//! stored or printed.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::ClaudeCode;
use crate::config::{AdapterKind, Config};

/// How to start the core.
#[derive(Debug, Clone)]
pub enum Launch<'a> {
    /// A new session whose first message is the text in this file.
    Fresh { first_message: &'a Path },
    /// Resume the pinned session by its explicit id.
    Resume { id: &'a str },
    /// A new session with no first message, under an id chosen up front
    /// (see [`CoreAdapter::new_session_id`]), so it is pinned before it has
    /// written anything.
    Blank { id: Option<&'a str> },
}

/// What the agent's screen in a pane says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Waiting for a message.
    Ready,
    /// Waiting for someone to answer a question first (the reason).
    Blocked(String),
    /// Can't tell from the screen.
    Unknown,
}

/// Single-quote for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub trait CoreAdapter {
    fn name(&self) -> &'static str;
    /// The shell line typed into the pane to start the core.
    fn launch_line(&self, command: &str, launch: &Launch) -> String;
    /// Whether [`Launch::Fresh`] already carries the first message. If not,
    /// the runner types a short pointer to the first-message file instead.
    fn first_message_in_launch(&self) -> bool;
    /// Whether sessions have ids that can be pinned and resumed.
    fn has_sessions(&self) -> bool;
    /// Typed (then Enter) to make the core exit back to the shell.
    fn exit_text(&self) -> &'static str {
        "/exit"
    }
    /// The id of a session in `cwd` that started at or after `since`, newest
    /// first, never `not`.
    fn find_session(&self, cwd: &Path, since: DateTime<Utc>, not: Option<&str>) -> Option<String>;
    /// The transcript of session `id` in `cwd`.
    fn transcript(&self, cwd: &Path, id: &str) -> Option<PathBuf>;
    /// An id for a session about to be started with [`Launch::Blank`], if
    /// the tool lets the caller choose it.
    fn new_session_id(&self) -> Option<String> {
        None
    }
    /// Read the pane's screen.
    fn screen(&self, _text: &str) -> Screen {
        Screen::Unknown
    }
    /// Will the tool run in a git worktree of `root` without asking whether
    /// to trust the folder? `None`: can't tell.
    fn workspace_trusted(&self, _root: &Path) -> Option<bool> {
        None
    }
}

/// Claude Code: one JSONL transcript per session under
/// `<projects>/<slug of cwd>/<session-id>.jsonl` (see [`crate::activity`]).
pub struct ClaudeCodeCore {
    pub projects: ClaudeCode,
    /// Claude Code's own config file (`~/.claude.json`), read for folder
    /// trust only. `None`: not known.
    pub config_file: Option<PathBuf>,
}

/// Claude Code's config file: `$CLAUDE_CONFIG_DIR/.claude.json`, else
/// `~/.claude.json`.
pub fn claude_config_file() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d).join(".claude.json")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude.json")),
    }
}

/// A random (version 4) UUID.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    let ok = File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !ok {
        // No /dev/urandom: time and pid are unique enough for a session id.
        let t = Utc::now().timestamp_nanos_opt().unwrap_or_default() as u128;
        let x = t ^ ((std::process::id() as u128) << 64);
        b = x.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The time of the first timestamped record in a JSONL file.
fn first_time(path: &Path) -> Option<DateTime<Utc>> {
    let f = File::open(path).ok()?;
    for line in BufReader::new(f).lines().take(200) {
        let Ok(line) = line else { break };
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(t) = v.get("timestamp").and_then(Value::as_str).and_then(parse) {
                return Some(t);
            }
        }
    }
    None
}

pub(crate) fn parse(t: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

impl CoreAdapter for ClaudeCodeCore {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn launch_line(&self, command: &str, launch: &Launch) -> String {
        match launch {
            // The message is read from the file by the shell, so only this
            // short line is typed, however long the message is. `--` ends
            // the options, so a variadic flag in `command` (such as
            // `--allowedTools a b`) can't swallow the message.
            Launch::Fresh { first_message } => format!(
                "{command} -- \"$(cat {})\"",
                shell_quote(&first_message.to_string_lossy())
            ),
            // Always the explicit id: `--continue` picks the most recently
            // touched transcript, which can be the wrong session.
            Launch::Resume { id } => format!("{command} --resume {}", shell_quote(id)),
            Launch::Blank { id: Some(id) } => {
                format!("{command} --session-id {}", shell_quote(id))
            }
            Launch::Blank { id: None } => command.to_string(),
        }
    }

    fn first_message_in_launch(&self) -> bool {
        true
    }

    fn has_sessions(&self) -> bool {
        true
    }

    fn find_session(&self, cwd: &Path, since: DateTime<Utc>, not: Option<&str>) -> Option<String> {
        // Allow a little clock slack between us and the tool.
        let since = since - chrono::Duration::seconds(2);
        let mut best: Option<(DateTime<Utc>, String)> = None;
        for dir in self.projects.dirs_for(cwd) {
            let Ok(rd) = fs::read_dir(dir) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                    continue;
                };
                if Some(id.as_str()) == not {
                    continue;
                }
                let Some(t) = first_time(&p) else { continue };
                if t >= since && best.as_ref().is_none_or(|(b, _)| t > *b) {
                    best = Some((t, id));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    fn transcript(&self, cwd: &Path, id: &str) -> Option<PathBuf> {
        self.projects
            .dirs_for(cwd)
            .into_iter()
            .map(|d| d.join(format!("{id}.jsonl")))
            .find(|p| p.is_file())
    }

    fn new_session_id(&self) -> Option<String> {
        Some(uuid_v4())
    }

    fn screen(&self, text: &str) -> Screen {
        if text.contains("Accessing workspace") || text.contains("trust this folder") {
            return Screen::Blocked(
                "Claude Code is asking whether to trust this folder".to_string(),
            );
        }
        if text.lines().any(|l| l.trim_start().starts_with('❯')) {
            return Screen::Ready;
        }
        Screen::Unknown
    }

    /// Claude Code keeps one config entry per *canonical* git root: a
    /// worktree's entry is its main checkout's. So a shard's worktree is
    /// trusted exactly when the memory root is (checked in Claude Code
    /// 2.1.319). This only reads the file; it never writes it.
    fn workspace_trusted(&self, root: &Path) -> Option<bool> {
        let text = fs::read_to_string(self.config_file.as_ref()?).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let key = root.to_string_lossy();
        Some(
            v.pointer("/projects")
                .and_then(|p| p.get(key.as_ref()))
                .and_then(|p| p.get("hasTrustDialogAccepted"))
                .and_then(Value::as_bool)
                == Some(true),
        )
    }
}

/// Any program that reads lines: no session ids, nothing to resume.
pub struct PlainCore;

impl CoreAdapter for PlainCore {
    fn name(&self) -> &'static str {
        "plain"
    }
    fn launch_line(&self, command: &str, _: &Launch) -> String {
        command.to_string()
    }
    fn first_message_in_launch(&self) -> bool {
        false
    }
    fn has_sessions(&self) -> bool {
        false
    }
    fn find_session(&self, _: &Path, _: DateTime<Utc>, _: Option<&str>) -> Option<String> {
        None
    }
    fn transcript(&self, _: &Path, _: &str) -> Option<PathBuf> {
        None
    }
}

/// The adapter of `kind` (`[core] adapter` or `[shards] adapter`).
pub fn adapter_for(kind: AdapterKind, cfg: &Config, root: &Path) -> Box<dyn CoreAdapter> {
    match kind {
        AdapterKind::ClaudeCode => Box::new(ClaudeCodeCore {
            projects: ClaudeCode::from_config(&cfg.activity, root),
            config_file: match cfg.shards.claude_config.trim() {
                "" => claude_config_file(),
                p => Some(PathBuf::from(p)),
            },
        }),
        AdapterKind::Plain => Box::new(PlainCore),
    }
}

/// Token counts of one model call, or summed over a reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl Usage {
    fn from_json(u: &Value) -> Usage {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Usage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_creation: n("cache_creation_input_tokens"),
        }
    }

    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_creation += o.cache_creation;
    }
}

/// One prompt and the core's reply to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// This turn's prompt number in the transcript (1-based).
    pub seq: u64,
    /// When the prompt was recorded.
    pub prompt_at: String,
    /// The prompt was exactly a `[HH:MM]` heartbeat.
    pub heartbeat: bool,
    /// The reply's first model call: its cache read vs creation says
    /// whether the prompt cache was warm.
    pub first: Option<Usage>,
    /// Summed over every model call in the reply.
    pub total: Usage,
    pub calls: u32,
    pub done: bool,
    /// Message id -> usage, while the turn is open (records of one message
    /// repeat its usage; the last one wins). Emptied when the turn ends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    open: Vec<(String, Usage)>,
}

impl Turn {
    fn close(&mut self) {
        self.total = Usage::default();
        for (_, u) in &self.open {
            self.total.add(u);
        }
        self.calls = self.open.len() as u32;
        self.first = self.open.first().map(|(_, u)| *u);
        self.open.clear();
        self.done = true;
    }
}

/// Completed turns kept.
const KEEP_TURNS: usize = 64;

/// Incremental read of the core's transcript: times and numbers only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptScan {
    pub path: String,
    pub offset: u64,
    /// Newest timestamp of any entry (the time *inside* the transcript, not
    /// the file's mtime, which bookkeeping writes can move).
    pub last_entry: Option<String>,
    /// Newest prompt of any kind.
    pub last_prompt: Option<String>,
    /// Newest prompt that was not a heartbeat: real activity.
    pub last_real: Option<String>,
    /// Newest "queued while busy" record.
    pub last_queued: Option<String>,
    /// Prompts seen so far.
    pub prompts: u64,
    /// "Queued while busy" records seen so far.
    pub queued: u64,
    pub turns: Vec<Turn>,
}

/// Is this prompt exactly a `[HH:MM]` heartbeat?
pub fn is_heartbeat(text: &str) -> bool {
    let b = text.trim().as_bytes();
    b.len() == 7
        && b[0] == b'['
        && b[3] == b':'
        && b[6] == b']'
        && [1, 2, 4, 5].iter().all(|&i| b[i].is_ascii_digit())
}

fn later(a: &str, b: Option<&str>) -> bool {
    match (parse(a), b.and_then(parse)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

fn bump(slot: &mut Option<String>, at: &str) {
    if later(at, slot.as_deref()) {
        *slot = Some(at.to_string());
    }
}

/// The text of a user record if it is a prompt (not a tool result, not a
/// tool's own bookkeeping message).
fn prompt_text(v: &Value) -> Option<String> {
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let c = v.pointer("/message/content")?;
    let text = if let Some(s) = c.as_str() {
        s.to_string()
    } else {
        let arr = c.as_array()?;
        if arr
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            return None;
        }
        arr.iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let t = text.trim_start();
    if t.starts_with("<command-") || t.starts_with("<local-command") {
        return None;
    }
    Some(text)
}

impl TranscriptScan {
    /// Read whatever is new in `path` (restarting if the path changed or the
    /// file shrank).
    pub fn update(&mut self, path: &Path) {
        let key = path.to_string_lossy().into_owned();
        if self.path != key {
            *self = TranscriptScan {
                path: key,
                ..Default::default()
            };
        }
        let Ok(mut f) = File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if self.offset > len {
            *self = TranscriptScan {
                path: self.path.clone(),
                ..Default::default()
            };
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut r = BufReader::new(f);
        let mut line = Vec::new();
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if line.last() != Some(&b'\n') {
                        break;
                    }
                    self.offset += n as u64;
                    if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                        self.fold(&v);
                    }
                }
            }
        }
    }

    fn open_turn(&mut self) -> Option<&mut Turn> {
        self.turns.last_mut().filter(|t| !t.done)
    }

    fn fold(&mut self, v: &Value) {
        let Some(at) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .filter(|t| parse(t).is_some())
        else {
            return;
        };
        bump(&mut self.last_entry, at);
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(heartbeat) = prompt_text(v).map(|t| is_heartbeat(&t)) else {
                    return;
                };
                if let Some(t) = self.open_turn() {
                    t.close();
                }
                bump(&mut self.last_prompt, at);
                if !heartbeat {
                    bump(&mut self.last_real, at);
                }
                self.prompts += 1;
                self.turns.push(Turn {
                    seq: self.prompts,
                    prompt_at: at.to_string(),
                    heartbeat,
                    ..Default::default()
                });
                if self.turns.len() > KEEP_TURNS {
                    let drop = self.turns.len() - KEEP_TURNS;
                    self.turns.drain(..drop);
                }
            }
            Some("queue-operation") => {
                if v.get("operation").and_then(Value::as_str) == Some("enqueue") {
                    bump(&mut self.last_queued, at);
                    self.queued += 1;
                }
            }
            Some("assistant") => {
                let Some(m) = v.get("message") else { return };
                let id = m
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let usage = m.get("usage").map(Usage::from_json);
                let end = m.get("stop_reason").and_then(Value::as_str) == Some("end_turn");
                let Some(t) = self.open_turn() else { return };
                if let Some(u) = usage {
                    match t.open.iter_mut().find(|(i, _)| *i == id) {
                        Some(slot) => slot.1 = u,
                        None => t.open.push((id, u)),
                    }
                }
                if end {
                    t.close();
                }
            }
            _ => {}
        }
    }

    /// Has a prompt (or a queued prompt) arrived since the scan stood at
    /// `(prompts, queued)`?
    pub fn landed_since(&self, before: (u64, u64)) -> bool {
        self.prompts > before.0 || self.queued > before.1
    }

    /// The first finished turn whose prompt came after prompt number `after`.
    pub fn reply_after(&self, after: u64) -> Option<&Turn> {
        self.turns.iter().find(|t| t.done && t.seq > after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn heartbeat_is_exactly_hh_mm() {
        assert!(is_heartbeat("[09:41]"));
        assert!(is_heartbeat("[23:05]\n"));
        assert!(!is_heartbeat("[9:41]"));
        assert!(!is_heartbeat("[09:41] hi"));
        assert!(!is_heartbeat("[WAKE] 1 event"));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        let c = ClaudeCodeCore {
            projects: ClaudeCode {
                projects: PathBuf::from("/nowhere"),
            },
            config_file: None,
        };
        assert_eq!(
            c.launch_line("claude --model haiku", &Launch::Resume { id: "abc" }),
            "claude --model haiku --resume 'abc'"
        );
        let l = c.launch_line(
            "claude",
            &Launch::Fresh {
                first_message: Path::new("/m/.shards/core/first.md"),
            },
        );
        assert_eq!(l, "claude -- \"$(cat '/m/.shards/core/first.md')\"");
        assert!(!l.contains("--continue"));
        assert_eq!(
            c.launch_line("claude", &Launch::Blank { id: Some("u-1") }),
            "claude --session-id 'u-1'"
        );
    }

    #[test]
    fn uuids_are_v4() {
        let a = uuid_v4();
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]));
        assert_ne!(a, uuid_v4());
    }

    #[test]
    fn screens_and_trust() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("claude.json");
        let root = dir.path().join("m");
        fs::create_dir(&root).unwrap();
        let c = ClaudeCodeCore {
            projects: ClaudeCode {
                projects: dir.path().join("p"),
            },
            config_file: Some(cfg.clone()),
        };
        let trust = " Accessing workspace:\n /m\n ❯ No, exit\n   Yes, I trust this folder\n";
        assert!(matches!(c.screen(trust), Screen::Blocked(_)));
        assert_eq!(c.screen("──────\n❯ \n──────\n"), Screen::Ready);
        assert_eq!(c.screen("starting…"), Screen::Unknown);
        assert_eq!(c.workspace_trusted(&root), None);
        let real = fs::canonicalize(&root).unwrap();
        fs::write(&cfg, r#"{"projects":{}}"#).unwrap();
        assert_eq!(c.workspace_trusted(&root), Some(false));
        let v = serde_json::json!({"projects": {real.to_string_lossy(): {"hasTrustDialogAccepted": true}}});
        fs::write(&cfg, v.to_string()).unwrap();
        assert_eq!(c.workspace_trusted(&root), Some(true));
    }

    #[test]
    fn scan_turns_usage_and_heartbeats() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut f = File::create(&p).unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"PLANTED first prompt"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-04T10:00:02.000Z","message":{"id":"m1","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":900},"stop_reason":null,"content":[{"type":"text","text":"PLANTED"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-04T10:00:03.000Z","message":{"id":"m1","usage":{"input_tokens":5,"output_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":900},"stop_reason":"tool_use","content":[]}}"#,
            r#"{"type":"user","timestamp":"2026-11-04T10:00:04.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"PLANTED"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-04T10:00:05.000Z","message":{"id":"m2","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":900,"cache_creation_input_tokens":10},"stop_reason":"end_turn","content":[]}}"#,
            r#"{"type":"last-prompt"}"#,
            r#"{"type":"user","timestamp":"2026-11-04T11:00:00.000Z","message":{"role":"user","content":"[11:00]"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-04T11:00:01.000Z","message":{"id":"m3","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":910,"cache_creation_input_tokens":0},"stop_reason":"end_turn","content":[]}}"#,
        ];
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        write!(f, r#"{{"type":"user","timestamp":"2026-11-04T12:00"#).unwrap();
        drop(f);
        let mut s = TranscriptScan::default();
        s.update(&p);
        assert_eq!(s.turns.len(), 2);
        let t = &s.turns[0];
        assert!(t.done && !t.heartbeat);
        assert_eq!(t.calls, 2);
        assert_eq!(t.first.unwrap().cache_creation, 900);
        assert_eq!(t.total.output, 10);
        assert_eq!(t.total.cache_read, 900);
        assert!(s.turns[1].heartbeat);
        assert_eq!(s.last_real.as_deref(), Some("2026-11-04T10:00:00.000Z"));
        assert_eq!(s.last_entry.as_deref(), Some("2026-11-04T11:00:01.000Z"));
        let state = serde_json::to_string(&s).unwrap();
        assert!(!state.contains("PLANTED"));
        assert_eq!(s.prompts, 2);
        assert_eq!(s.reply_after(0).unwrap().seq, 1);
        assert!(s.reply_after(1).unwrap().heartbeat);
        assert!(s.reply_after(2).is_none());
        assert!(s.landed_since((1, 0)) && !s.landed_since((2, 0)));
        // The half-written last line is read once complete.
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#":00.000Z","message":{{"content":"later"}}}}"#).unwrap();
        s.update(&p);
        assert_eq!(s.last_real.as_deref(), Some("2026-11-04T12:00:00.000Z"));
    }
}
