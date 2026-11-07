// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Known quirks: a shop-wide board of what is weird about a tool right now.
//!
//! # Where quirks live
//!
//! One file per quirk, `quirks/<id>.md`, committed **directly to `main`**
//! from whatever worktree the writer is in (main or any shard). A quirk never
//! goes through a shard branch, a dinit or the core:
//!
//! - **Visible at once.** Every read goes to the `main` ref, which all
//!   worktrees share, so a quirk posted from shard A is listed by shard B on
//!   its very next `mem quirk list`, with no dinit and no merge.
//! - **No lost writes.** Every write takes the main lock for `quirks/`
//!   (`main` with store scope, `main.quirks` with section scope) and the git
//!   mutex, then reads, allocates the next id and commits under them. Two
//!   writers at the same moment simply queue.
//! - **History for free.** A status change rewrites the file as one new
//!   commit on main, like a supersede; the old status stays in git history
//!   with its own time and author. Nothing is ever deleted: archiving only
//!   changes `state`.
//! - **Dinit never sees them.** Shards don't change `quirks/` on their own
//!   branch, and dinit builds its merge from main's tree, so quirks on main
//!   survive every merge.
//!
//! Duplicate reports are allowed; tidying them is left to a later core task.
//!
//! # File format
//!
//! ```text
//! ---
//! id: q3
//! tool: bandsaw-2
//! quirk: the file handle is loose
//! reported_by: Bob
//! reported: 2026-11-07T10:00:00Z
//! state: open
//! status: open
//! status_by: Bob
//! status_at: 2026-11-07T10:00:00Z
//! ---
//! Optional free-text details.
//! ```
//!
//! `state` is what the code acts on (`open`, `fixed`, `archived`); `status`
//! is the human words. Optional keys: `fixed_at`, `last_mentioned`,
//! `archived_at`.

use chrono::{DateTime, Duration as ChronoDuration};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::memory::{now, Context, FileWrite, Memory, Provenance};

/// The folder on main that holds quirks.
pub const DIR: &str = "quirks";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuirkState {
    /// Still weird. Never archived.
    Open,
    /// Fixed; archived once nobody has mentioned it for N days.
    Fixed,
    /// Fixed and quiet for N days. Still on record.
    Archived,
}

impl QuirkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuirkState::Open => "open",
            QuirkState::Fixed => "fixed",
            QuirkState::Archived => "archived",
        }
    }

    fn parse(s: &str) -> Option<QuirkState> {
        match s {
            "open" => Some(QuirkState::Open),
            "fixed" => Some(QuirkState::Fixed),
            "archived" => Some(QuirkState::Archived),
            _ => None,
        }
    }
}

/// One quirk as it stands on main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Quirk {
    pub id: String,
    pub tool: String,
    pub quirk: String,
    pub reported_by: String,
    /// When it was first reported (RFC 3339 UTC, set automatically).
    pub reported: String,
    pub state: QuirkState,
    /// Last known status, in words: "open", "fixed by Ada", "worse when cold".
    pub status: String,
    pub status_by: String,
    pub status_at: String,
    pub fixed_at: Option<String>,
    pub last_mentioned: Option<String>,
    pub archived_at: Option<String>,
    pub details: Option<String>,
}

/// One version of a quirk, from git history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuirkVersion {
    pub commit: String,
    /// `quirk-add`, `quirk-update`, `quirk-fix`, `quirk-mention`, `quirk-archive`.
    pub op: String,
    /// The shard or session that made the commit.
    pub author: String,
    pub quirk: Quirk,
}

impl QuirkVersion {
    /// When this version was made: the mention, archive or status time.
    pub fn at(&self) -> &str {
        let q = &self.quirk;
        match self.op.as_str() {
            "quirk-mention" => q.last_mentioned.as_deref().unwrap_or(&q.status_at),
            "quirk-archive" => q.archived_at.as_deref().unwrap_or(&q.status_at),
            _ => &q.status_at,
        }
    }
}

/// What to post.
#[derive(Debug, Clone, Default)]
pub struct NewQuirk {
    pub tool: String,
    pub quirk: String,
    pub by: String,
    pub details: Option<String>,
}

fn path_of(id: &str) -> String {
    format!("{DIR}/{id}.md")
}

/// Accept `q3` or `3`.
pub fn normalize_id(id: &str) -> Result<String> {
    let n = id.strip_prefix('q').unwrap_or(id);
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
        return Err(Error::UnknownQuirk(id.to_string()));
    }
    Ok(format!("q{n}"))
}

fn id_num(id: &str) -> u64 {
    id.strip_prefix('q')
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn one_line(field: &'static str, v: &str) -> Result<()> {
    if v.trim().is_empty() {
        return Err(Error::BadQuirk(format!("{field} must not be empty")));
    }
    if v.contains('\n') || v.contains('\r') {
        return Err(Error::Multiline { field });
    }
    Ok(())
}

impl Quirk {
    pub fn render(&self) -> String {
        let mut s = String::from("---\n");
        let mut kv = |k: &str, v: &str| s += &format!("{k}: {v}\n");
        kv("id", &self.id);
        kv("tool", &self.tool);
        kv("quirk", &self.quirk);
        kv("reported_by", &self.reported_by);
        kv("reported", &self.reported);
        kv("state", self.state.as_str());
        kv("status", &self.status);
        kv("status_by", &self.status_by);
        kv("status_at", &self.status_at);
        for (k, v) in [
            ("fixed_at", &self.fixed_at),
            ("last_mentioned", &self.last_mentioned),
            ("archived_at", &self.archived_at),
        ] {
            if let Some(v) = v {
                kv(k, v);
            }
        }
        s += "---\n";
        if let Some(d) = self.details.as_deref().filter(|d| !d.trim().is_empty()) {
            s += d.trim_end_matches('\n');
            s += "\n";
        }
        s
    }

    pub fn parse(text: &str, path: &str) -> Result<Quirk> {
        let bad = |reason: &str| Error::Malformed {
            path: path.to_string(),
            reason: reason.to_string(),
        };
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| bad("no front matter"))?;
        let end = rest
            .find("\n---\n")
            .map(|i| (i, i + 5))
            .or_else(|| rest.strip_suffix("\n---").map(|r| (r.len(), rest.len())))
            .ok_or_else(|| bad("unterminated front matter"))?;
        let (head, body) = (&rest[..end.0], &rest[end.1..]);
        let mut get = std::collections::BTreeMap::new();
        for line in head.lines() {
            let (k, v) = line
                .split_once(": ")
                .or_else(|| line.strip_suffix(':').map(|k| (k, "")))
                .ok_or_else(|| bad(&format!("bad line `{line}`")))?;
            get.insert(k.to_string(), v.to_string());
        }
        let need = |k: &str| {
            get.get(k)
                .cloned()
                .ok_or_else(|| bad(&format!("missing `{k}`")))
        };
        let opt = |k: &str| get.get(k).cloned().filter(|v| !v.is_empty());
        let state = need("state")?;
        Ok(Quirk {
            id: need("id")?,
            tool: need("tool")?,
            quirk: need("quirk")?,
            reported_by: need("reported_by")?,
            reported: need("reported")?,
            state: QuirkState::parse(&state)
                .ok_or_else(|| bad(&format!("unknown state `{state}`")))?,
            status: need("status")?,
            status_by: need("status_by")?,
            status_at: need("status_at")?,
            fixed_at: opt("fixed_at"),
            last_mentioned: opt("last_mentioned"),
            archived_at: opt("archived_at"),
            details: Some(body.trim_end_matches('\n').to_string()).filter(|d| !d.is_empty()),
        })
    }

    /// When the archive clock last started: the later of the fix and the
    /// last mention.
    pub fn quiet_since(&self) -> Option<&str> {
        match (self.fixed_at.as_deref(), self.last_mentioned.as_deref()) {
            (Some(f), Some(m)) => Some(if later(m, f) { m } else { f }),
            (f, m) => f.or(m),
        }
    }
}

/// Is RFC 3339 `a` later than `b`? Unparsable times compare as text.
fn later(a: &str, b: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(a),
        DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => a > b,
    }
}

/// One quirk write, computed under the lock.
struct Change {
    op: &'static str,
    subject: String,
    writes: Vec<FileWrite>,
    trailers: Vec<(&'static str, String)>,
}

impl Memory {
    /// Every quirk on main, oldest id first, read from the `main` ref (so a
    /// shard sees quirks posted a moment ago by any other shard).
    pub fn quirks(&self) -> Result<Vec<Quirk>> {
        self.quirks_at("main")
    }

    fn quirks_at(&self, rev: &str) -> Result<Vec<Quirk>> {
        let out = self.git.run(&[
            "ls-tree",
            "-r",
            "--name-only",
            rev,
            "--",
            &format!("{DIR}/"),
        ])?;
        let mut v = Vec::new();
        for p in out.lines().filter(|p| p.ends_with(".md")) {
            if let Some(text) = self.read_file_at(rev, p)? {
                v.push(Quirk::parse(&text, p)?);
            }
        }
        v.sort_by_key(|q| id_num(&q.id));
        Ok(v)
    }

    /// One quirk as it stands on main.
    pub fn quirk(&self, id: &str) -> Result<Quirk> {
        let id = normalize_id(id)?;
        self.quirk_at("main", &id)?.ok_or(Error::UnknownQuirk(id))
    }

    fn quirk_at(&self, rev: &str, id: &str) -> Result<Option<Quirk>> {
        let p = path_of(id);
        self.read_file_at(rev, &p)?
            .map(|t| Quirk::parse(&t, &p))
            .transpose()
    }

    /// Every version of a quirk, newest first.
    pub fn quirk_history(&self, id: &str) -> Result<Vec<QuirkVersion>> {
        let q = self.quirk(id)?;
        let p = path_of(&q.id);
        let log = self.git.run(&[
            "log",
            "--topo-order",
            "--format=%H%x09%an%x09%s",
            "main",
            "--",
            &p,
        ])?;
        let mut out = Vec::new();
        for line in log.lines() {
            let mut parts = line.splitn(3, '\t');
            let (commit, author, subject) = (
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
                parts.next().unwrap_or(""),
            );
            if let Some(quirk) = self.quirk_at(commit, &q.id)? {
                out.push(QuirkVersion {
                    commit: commit.to_string(),
                    op: subject.split(':').next().unwrap_or("").to_string(),
                    author: author.to_string(),
                    quirk,
                });
            }
        }
        Ok(out)
    }

    /// Post a new quirk to main. Returns it with its id and report time.
    pub fn quirk_add(&self, new: &NewQuirk, prov: &Provenance) -> Result<Quirk> {
        one_line("tool", &new.tool)?;
        one_line("quirk", &new.quirk)?;
        one_line("by", &new.by)?;
        let mut added = None;
        self.quirk_commit(prov, &new.by, |main, at| {
            let next = main
                .quirks_at("HEAD")?
                .iter()
                .map(|q| id_num(&q.id))
                .max()
                .unwrap_or(0)
                + 1;
            let q = Quirk {
                id: format!("q{next}"),
                tool: new.tool.trim().to_string(),
                quirk: new.quirk.trim().to_string(),
                reported_by: new.by.trim().to_string(),
                reported: at.to_string(),
                state: QuirkState::Open,
                status: "open".to_string(),
                status_by: new.by.trim().to_string(),
                status_at: at.to_string(),
                fixed_at: None,
                last_mentioned: None,
                archived_at: None,
                details: new.details.clone().filter(|d| !d.trim().is_empty()),
            };
            let ch = Change {
                op: "quirk-add",
                subject: format!("{} ({})", q.id, q.tool),
                writes: vec![(path_of(&q.id), Some(q.render()))],
                trailers: vec![("Quirk", q.id.clone()), ("Tool", q.tool.clone())],
            };
            added = Some(q);
            Ok(Some(ch))
        })?;
        Ok(added.expect("committed"))
    }

    /// Set a quirk's status in words. The quirk is (re)opened; the old status
    /// stays in history.
    pub fn quirk_update(
        &self,
        id: &str,
        status: &str,
        by: &str,
        prov: &Provenance,
    ) -> Result<Quirk> {
        one_line("status", status)?;
        one_line("by", by)?;
        self.quirk_change(id, by, prov, "quirk-update", |q, at| {
            q.state = QuirkState::Open;
            q.status = status.trim().to_string();
            q.status_by = by.trim().to_string();
            q.status_at = at.to_string();
            q.fixed_at = None;
            q.archived_at = None;
        })
    }

    /// Mark a quirk fixed: status becomes "fixed by PERSON" and the archive
    /// clock starts.
    pub fn quirk_fix(&self, id: &str, by: &str, prov: &Provenance) -> Result<Quirk> {
        one_line("by", by)?;
        self.quirk_change(id, by, prov, "quirk-fix", |q, at| {
            q.state = QuirkState::Fixed;
            q.status = format!("fixed by {}", by.trim());
            q.status_by = by.trim().to_string();
            q.status_at = at.to_string();
            q.fixed_at = Some(at.to_string());
            q.archived_at = None;
        })
    }

    /// Record that a quirk came up again. Resets a fixed quirk's archive
    /// clock; brings an archived one back to fixed (with a fresh clock).
    pub fn quirk_mention(&self, id: &str, by: &str, prov: &Provenance) -> Result<Quirk> {
        self.quirk_change(id, by, prov, "quirk-mention", |q, at| {
            q.last_mentioned = Some(at.to_string());
            if q.state == QuirkState::Archived {
                q.state = QuirkState::Fixed;
                q.archived_at = None;
            }
        })
    }

    /// Archive every fixed quirk not mentioned (or fixed) for `days` days,
    /// as one commit. Open quirks are never archived; nothing is deleted.
    /// Returns the archived quirks (none: no commit).
    pub fn quirk_archive(&self, days: f64, prov: &Provenance) -> Result<Vec<Quirk>> {
        if !days.is_finite() || days < 0.0 {
            return Err(Error::BadQuirk(format!(
                "archive days must be >= 0, not {days}"
            )));
        }
        let by = prov.human.clone().unwrap_or_default();
        let mut done = Vec::new();
        self.quirk_commit(prov, &by, |main, at| {
            let now_t = DateTime::parse_from_rfc3339(at)
                .map_err(|e| Error::BadQuirk(format!("bad time `{at}`: {e}")))?;
            let cutoff = now_t - ChronoDuration::milliseconds((days * 86_400_000.0) as i64);
            for mut q in main.quirks_at("HEAD")? {
                if q.state != QuirkState::Fixed {
                    continue;
                }
                let quiet = q
                    .quiet_since()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
                if quiet.is_some_and(|t| t <= cutoff) {
                    q.state = QuirkState::Archived;
                    q.archived_at = Some(at.to_string());
                    done.push(q);
                }
            }
            if done.is_empty() {
                return Ok(None);
            }
            let ids: Vec<String> = done.iter().map(|q| q.id.clone()).collect();
            Ok(Some(Change {
                op: "quirk-archive",
                subject: ids.join(", "),
                writes: done
                    .iter()
                    .map(|q| (path_of(&q.id), Some(q.render())))
                    .collect(),
                trailers: vec![
                    ("Quirk", ids.join(", ")),
                    ("Archive-Days", format!("{days}")),
                ],
            }))
        })?;
        Ok(done)
    }

    fn quirk_change(
        &self,
        id: &str,
        by: &str,
        prov: &Provenance,
        op: &'static str,
        edit: impl FnOnce(&mut Quirk, &str),
    ) -> Result<Quirk> {
        let id = normalize_id(id)?;
        if by.contains('\n') || by.contains('\r') {
            return Err(Error::Multiline { field: "by" });
        }
        let mut out = None;
        self.quirk_commit(prov, by, |main, at| {
            let mut q = main
                .quirk_at("HEAD", &id)?
                .ok_or_else(|| Error::UnknownQuirk(id.clone()))?;
            edit(&mut q, at);
            let ch = Change {
                op,
                subject: format!("{} ({})", q.id, q.tool),
                writes: vec![(path_of(&q.id), Some(q.render()))],
                trailers: vec![("Quirk", q.id.clone()), ("Status", q.status.clone())],
            };
            out = Some(q);
            Ok(Some(ch))
        })?;
        Ok(out.expect("committed"))
    }

    /// Commit a quirk change straight to main, from any worktree: take the
    /// main lock for `quirks/` and the git mutex, then compute the change
    /// against main's HEAD (so ids and checks can't go stale) and commit it.
    fn quirk_commit(
        &self,
        prov: &Provenance,
        by: &str,
        prepare: impl FnOnce(&Memory, &str) -> Result<Option<Change>>,
    ) -> Result<Option<String>> {
        // Author rules are this worktree's: in a shard, the shard id.
        let mut prov = self.effective(prov, false)?;
        if !by.trim().is_empty() {
            prov.human = Some(by.trim().to_string());
        }
        let main = self.main()?;
        debug_assert_eq!(main.context(), &Context::Main);
        let lock_path = vec![format!("{DIR}/_")];
        let guards = main.lock_main(&lock_path, &prov.author, prov.human.as_deref())?;
        let _git = main.git_mutex()?;
        if prov.at.is_none() {
            prov.at = Some(now());
        }
        let at = prov.timestamp();
        let Some(ch) = prepare(&main, &at)? else {
            return Ok(None);
        };
        for g in &guards {
            g.ensure_held()?;
        }
        let trailers: Vec<(&str, String)> =
            ch.trailers.iter().map(|(k, v)| (*k, v.clone())).collect();
        main.commit_files(ch.op, &ch.subject, &ch.writes, &prov, &trailers)
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Quirk {
        Quirk {
            id: "q2".into(),
            tool: "bandsaw-2".into(),
            quirk: "file handle is loose: hold it".into(),
            reported_by: "Bob".into(),
            reported: "2026-11-07T10:00:00Z".into(),
            state: QuirkState::Fixed,
            status: "fixed by Ada".into(),
            status_by: "Ada".into(),
            status_at: "2026-11-08T10:00:00Z".into(),
            fixed_at: Some("2026-11-08T10:00:00Z".into()),
            last_mentioned: Some("2026-11-09T10:00:00Z".into()),
            archived_at: None,
            details: Some("line one\n---\nline three".into()),
        }
    }

    #[test]
    fn round_trip() {
        let q = sample();
        assert_eq!(Quirk::parse(&q.render(), "x").unwrap(), q);
        let mut bare = sample();
        bare.details = None;
        bare.fixed_at = None;
        assert_eq!(Quirk::parse(&bare.render(), "x").unwrap(), bare);
    }

    #[test]
    fn ids_and_clock() {
        assert_eq!(normalize_id("3").unwrap(), "q3");
        assert_eq!(normalize_id("q12").unwrap(), "q12");
        assert!(normalize_id("q").is_err());
        assert!(normalize_id("../x").is_err());
        assert!(normalize_id("q01").is_err());
        assert_eq!(sample().quiet_since(), Some("2026-11-09T10:00:00Z"));
    }
}
