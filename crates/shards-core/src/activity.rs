// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Observation: what a shard is actually doing, read from its agent's own
//! records rather than reported by the shard.
//!
//! An [`ActivitySource`] reads new activity for one shard and folds it into
//! that shard's [`ActivityState`], kept in `.shards/activity/<id>.json`. The
//! system is not tied to one agent tool: [`ClaudeCode`] reads Claude Code
//! session transcripts, [`NoSource`] reads nothing (git only).
//!
//! # Privacy
//!
//! Transcripts hold prompts, answers, file contents and tool results. None of
//! that is ever stored or printed. A source keeps only:
//!
//! - the time of the latest record (any kind), and
//! - per tool call, a summary: the tool name plus a file *basename*, or for a
//!   shell command at most 80 characters of it, after [`redact`] has removed
//!   bearer tokens, `password=`/`token=`/`api_key=` style assignments, JWTs,
//!   long hex strings and `user:pass@` URLs.
//!
//! Transcripts are read incrementally: per file the state keeps a byte
//! offset, and only complete new lines are read.
//!
//! # Where Claude Code keeps transcripts
//!
//! `~/.claude/projects/<slug>/<session-id>.jsonl` (or under
//! `$CLAUDE_CONFIG_DIR/projects`), where `<slug>` is the session's working
//! directory with *every* character other than `A-Z a-z 0-9` replaced by `-`
//! (so `/m/.shards/worktrees/s1.v2` becomes `-m--shards-worktrees-s1-v2`).
//! A slug longer than 200 characters is cut to 200 and gets `-<hash>`
//! appended; such directories are matched by prefix. A shard's worktree is
//! its session's working directory, so its transcripts are found from the
//! worktree path alone.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ActivityConfig, SourceKind};
use crate::error::Result;
use crate::lock::FileMutex;
use crate::memory::Memory;
use crate::shard::ShardMeta;

/// Recent actions kept per shard.
pub const KEEP_ACTIONS: usize = 20;
/// Longest command excerpt kept.
pub const MAX_COMMAND: usize = 80;
/// Claude Code's slug length limit before it appends a hash.
const SLUG_MAX: usize = 200;

/// One observed action, already summarized and redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// RFC 3339.
    pub at: String,
    /// The tool, e.g. `Edit` or `Bash`.
    pub tool: String,
    /// A file basename, a redacted command excerpt, or empty.
    pub summary: String,
}

/// How far a source has read one transcript file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub offset: u64,
}

/// `.shards/activity/<id>.json`: summaries and read offsets only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityState {
    pub shard: String,
    pub source: String,
    /// Latest record time seen in a transcript (RFC 3339).
    pub last_activity: Option<String>,
    /// Newest last, at most [`KEEP_ACTIONS`].
    pub recent: Vec<Action>,
    /// Per transcript file (absolute path): how far it has been read.
    #[serde(default)]
    pub files: BTreeMap<String, Cursor>,
}

impl ActivityState {
    fn push(&mut self, a: Action) {
        self.recent.push(a);
        if self.recent.len() > KEEP_ACTIONS {
            let drop = self.recent.len() - KEEP_ACTIONS;
            self.recent.drain(..drop);
        }
    }

    fn saw(&mut self, at: &str) {
        if self
            .last_activity
            .as_deref()
            .is_none_or(|cur| later(at, cur))
        {
            self.last_activity = Some(at.to_string());
        }
    }
}

/// Is RFC 3339 time `a` after `b`? (Unparsable times lose.)
fn later(a: &str, b: &str) -> bool {
    use chrono::DateTime;
    match (
        DateTime::parse_from_rfc3339(a),
        DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a > b,
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

/// A place shard activity can be read from. Implementations must never fail:
/// anything missing or unreadable means "no new activity".
pub trait ActivitySource {
    /// `claude-code`, `none`, ...
    fn name(&self) -> &'static str;
    /// Fold any new activity for `shard` into `state`.
    fn poll(&self, shard: &ShardMeta, state: &mut ActivityState);
}

/// Git only: no transcript.
pub struct NoSource;

impl ActivitySource for NoSource {
    fn name(&self) -> &'static str {
        "none"
    }
    fn poll(&self, _: &ShardMeta, _: &mut ActivityState) {}
}

/// Claude Code session transcripts.
pub struct ClaudeCode {
    /// The `projects` directory holding one directory per working directory.
    pub projects: PathBuf,
}

/// Claude Code's directory name for a working directory: every character
/// other than ASCII letters and digits becomes `-` (one per UTF-16 unit, as
/// in JavaScript).
pub fn claude_slug(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('-');
            }
        }
    }
    out
}

impl ClaudeCode {
    /// From config: `transcript_dir`, else `$CLAUDE_CONFIG_DIR/projects`,
    /// else `~/.claude/projects`.
    pub fn from_config(cfg: &ActivityConfig, root: &Path) -> ClaudeCode {
        let dir = cfg.transcript_dir.trim();
        let home = || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
        };
        let projects = if !dir.is_empty() {
            if let Some(rest) = dir.strip_prefix("~/") {
                home().join(rest)
            } else if Path::new(dir).is_absolute() {
                PathBuf::from(dir)
            } else {
                root.join(dir)
            }
        } else if let Some(c) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|c| !c.is_empty()) {
            PathBuf::from(c).join("projects")
        } else {
            home().join(".claude").join("projects")
        };
        ClaudeCode { projects }
    }

    /// Transcript directories for a worktree (the path as recorded and its
    /// canonical form; long slugs matched by prefix).
    pub fn dirs_for(&self, worktree: &Path) -> Vec<PathBuf> {
        let mut paths = vec![worktree.to_path_buf()];
        if let Ok(c) = fs::canonicalize(worktree) {
            if c != worktree {
                paths.push(c);
            }
        }
        let mut out = Vec::new();
        for p in paths {
            let slug = claude_slug(&p);
            if slug.len() <= SLUG_MAX {
                let d = self.projects.join(&slug);
                if d.is_dir() && !out.contains(&d) {
                    out.push(d);
                }
            } else {
                let prefix = format!("{}-", &slug[..SLUG_MAX]);
                if let Ok(rd) = fs::read_dir(&self.projects) {
                    for e in rd.filter_map(|e| e.ok()) {
                        let d = e.path();
                        if e.file_name().to_string_lossy().starts_with(&prefix)
                            && d.is_dir()
                            && !out.contains(&d)
                        {
                            out.push(d);
                        }
                    }
                }
            }
        }
        out
    }

    fn read_file(&self, path: &Path, state: &mut ActivityState) {
        let key = path.to_string_lossy().to_string();
        let Ok(mut f) = File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut offset = state.files.get(&key).map(|c| c.offset).unwrap_or(0);
        if offset > len {
            // Truncated or replaced: start over.
            offset = 0;
        }
        if f.seek(SeekFrom::Start(offset)).is_err() {
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
                        // Partial line still being written: read it next time.
                        break;
                    }
                    offset += n as u64;
                    if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                        fold_record(&v, state);
                    }
                }
            }
        }
        state.files.insert(key, Cursor { offset });
    }
}

impl ActivitySource for ClaudeCode {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn poll(&self, shard: &ShardMeta, state: &mut ActivityState) {
        let mut files: Vec<PathBuf> = self
            .dirs_for(&shard.worktree)
            .into_iter()
            .filter_map(|d| fs::read_dir(d).ok())
            .flat_map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
            .collect();
        // Oldest first, so recent actions end up in time order.
        files.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        for f in files {
            self.read_file(&f, state);
        }
    }
}

/// Fold one transcript record into the state, keeping only a timestamp and
/// tool-call summaries. Nothing else from the record is retained.
fn fold_record(v: &Value, state: &mut ActivityState) {
    let Some(at) = v.get("timestamp").and_then(Value::as_str) else {
        return;
    };
    if chrono::DateTime::parse_from_rfc3339(at).is_err() {
        return;
    }
    state.saw(at);
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for c in content {
        if c.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(tool) = c.get("name").and_then(Value::as_str) else {
            continue;
        };
        let tool: String = tool
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "_-.".contains(*c))
            .take(40)
            .collect();
        let input = c.get("input");
        state.push(Action {
            at: at.to_string(),
            summary: summarize(&tool, input),
            tool,
        });
    }
}

/// The only things kept from a tool call's input.
fn summarize(tool: &str, input: Option<&Value>) -> String {
    let get = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    if let Some(cmd) = get("command").filter(|_| tool == "Bash" || tool == "PowerShell") {
        return command_excerpt(cmd);
    }
    for k in ["file_path", "notebook_path", "path"] {
        if let Some(p) = get(k) {
            let base = p.rsplit(['/', '\\']).next().unwrap_or("");
            return redact(base).chars().take(MAX_COMMAND).collect();
        }
    }
    String::new()
}

/// One line, redacted, at most [`MAX_COMMAND`] characters. Redaction runs
/// before truncation, so a cut can't leave half a secret behind.
pub fn command_excerpt(cmd: &str) -> String {
    let one_line = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    let red = redact(&one_line);
    if red.chars().count() <= MAX_COMMAND {
        red
    } else {
        let mut s: String = red.chars().take(MAX_COMMAND - 1).collect();
        s.push('…');
        s
    }
}

fn redactors() -> &'static [(Regex, &'static str)] {
    static R: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    R.get_or_init(|| {
        [
            // user:pass@ in URLs
            (r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]*@", "${1}[REDACTED]@"),
            // Authorization headers
            (r"(?i)\b(bearer|basic|token)\s+[A-Za-z0-9._~+/=-]{6,}", "$1 [REDACTED]"),
            // JWTs
            (r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*", "[REDACTED]"),
            // password=..., API_KEY: ..., "token": "..."
            (
                r#"(?i)([A-Za-z0-9_-]*(?:passw(?:or)?d|pwd|passphrase|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|credential)[A-Za-z0-9_-]*["']?\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s"',;&|]+)"#,
                "${1}[REDACTED]",
            ),
            // --password x, -p=x style flags
            (
                r"(?i)(--?(?:passw(?:or)?d|pwd|secret|token|api[_-]?key|apikey|auth)[ =])\S+",
                "${1}[REDACTED]",
            ),
            // Well-known token prefixes
            (
                r"\b(?:sk|pk|rk|ghp|gho|ghu|ghs|ghr|github_pat|glpat|xox[abprs])[-_][A-Za-z0-9_-]{8,}",
                "[REDACTED]",
            ),
            (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
            // Long hex (keys, hashes)
            (r"\b[0-9A-Fa-f]{32,}\b", "[REDACTED]"),
        ]
        .into_iter()
        .map(|(re, to)| (Regex::new(re).expect("valid regex"), to))
        .collect()
    })
}

/// Remove secrets from a string.
pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for (re, to) in redactors() {
        out = re.replace_all(&out, *to).into_owned();
    }
    out
}

/// The configured source.
pub fn source_for(cfg: &ActivityConfig, root: &Path) -> Box<dyn ActivitySource> {
    match cfg.source {
        SourceKind::ClaudeCode => Box::new(ClaudeCode::from_config(cfg, root)),
        SourceKind::None => Box::new(NoSource),
    }
}

impl Memory {
    pub(crate) fn activity_dir(&self) -> PathBuf {
        self.state_dir().join("activity")
    }

    fn activity_path(&self, id: &str) -> PathBuf {
        self.activity_dir().join(format!("{id}.json"))
    }

    /// The stored activity for a shard, without reading anything new.
    pub fn activity_state(&self, id: &str) -> ActivityState {
        fs::read_to_string(self.activity_path(id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| ActivityState {
                shard: id.to_string(),
                ..Default::default()
            })
    }

    /// Read new activity for a shard from the configured source and save it.
    /// Never fails because a transcript is missing or unreadable; only a
    /// failure to write `.shards/activity/` is an error. Takes only a
    /// per-shard flock on `.shards/activity/<id>.guard`, never a main lock.
    pub fn observe(&self, meta: &ShardMeta) -> Result<ActivityState> {
        let source = source_for(&self.config.activity, &self.main_root);
        let dir = self.activity_dir();
        let _guard = FileMutex::lock(&dir.join(format!("{}.guard", meta.id)))?;
        let mut state = self.activity_state(&meta.id);
        if state.source != source.name() {
            // Source changed: offsets mean nothing any more.
            state = ActivityState {
                shard: meta.id.clone(),
                source: source.name().to_string(),
                ..Default::default()
            };
        }
        let before = state.clone();
        source.poll(meta, &mut state);
        if state != before || !self.activity_path(&meta.id).exists() {
            let path = self.activity_path(&meta.id);
            let tmp = dir.join(format!(".{}.json.tmp", meta.id));
            fs::write(
                &tmp,
                serde_json::to_string_pretty(&state).expect("serializable"),
            )?;
            fs::rename(tmp, path)?;
        }
        Ok(state)
    }

    /// Drop a closed shard's activity state.
    pub(crate) fn forget_activity(&self, id: &str) {
        let _ = fs::remove_file(self.activity_path(id));
        let _ = fs::remove_file(self.activity_dir().join(format!("{id}.guard")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(
            claude_slug(Path::new("/home/ada/src/flock-memory")),
            "-home-ada-src-flock-memory"
        );
        assert_eq!(
            claude_slug(Path::new("/tmp/verify-x/.shards/worktrees/s1.v2")),
            "-tmp-verify-x--shards-worktrees-s1-v2"
        );
        assert_eq!(claude_slug(Path::new("/a b/é/x_y")), "-a-b---x-y");
    }

    #[test]
    fn redaction() {
        let cases = [
            (
                "curl -H 'Authorization: Bearer abcDEF123456789xyz' https://x",
                "abcDEF123456789xyz",
            ),
            ("export API_KEY=sekrit-value-1", "sekrit-value-1"),
            ("mysql --password hunter22 db", "hunter22"),
            ("PASSWORD='p a s s' ./run", "p a s s"),
            (
                "git clone https://ada:hunter22@git.example.invalid/r",
                "hunter22",
            ),
            (
                "echo eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJl",
                "eyJzdWIiOiJ4In0",
            ),
            (
                "x 0123456789abcdef0123456789abcdef01234567",
                "0123456789abcdef0123",
            ),
            (
                "gh auth --with-token ghp_AbCdEfGhIjKlMnOp1234",
                "ghp_AbCdEfGhIjKlMnOp1234",
            ),
            (r#"{"token": "tok-999-secret"}"#, "tok-999-secret"),
        ];
        for (input, secret) in cases {
            let out = redact(input);
            assert!(!out.contains(secret), "{input} -> {out}");
            assert!(out.contains("[REDACTED]"), "{input} -> {out}");
        }
        assert_eq!(redact("cargo test --release"), "cargo test --release");
    }

    #[test]
    fn excerpt_is_short_one_line_and_redacted_before_cut() {
        let long = format!("{} TOKEN=abcdefghijklmnop", "x".repeat(70));
        let e = command_excerpt(&long);
        assert!(e.chars().count() <= MAX_COMMAND);
        assert!(!e.contains("abcdefgh"));
        assert_eq!(command_excerpt("a\n  b\tc"), "a b c");
    }

    #[test]
    fn summaries_keep_only_tool_and_basename() {
        let v: Value = serde_json::json!({
            "file_path": "/secret/dir/notes.md", "content": "PLANTED"
        });
        assert_eq!(summarize("Write", Some(&v)), "notes.md");
        let v: Value = serde_json::json!({"pattern": "PLANTED", "path": "/a/b"});
        assert_eq!(summarize("Grep", Some(&v)), "b");
        let v: Value = serde_json::json!({"prompt": "PLANTED"});
        assert_eq!(summarize("Agent", Some(&v)), "");
    }
}
