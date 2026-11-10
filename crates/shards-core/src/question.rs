// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Open questions: a held change, turned into something to ask the next
//! person who can answer it.
//!
//! When the core holds a change in a proposal, it must say what to ask
//! (`mem proposal hold <id> --change N --why "..." --ask "..."`). The hold
//! writes a question, `questions/ask<n>.json` on main, in the same commit:
//! the ask, the change it is about (`p3#2`), the value, who said it and why,
//! and why the core held it.
//!
//! A shard lists the open questions its person may answer in its first
//! context (`mem shard context`) and asks one near the start of a session.
//! - The person knows: the shard records it as a normal fact change citing
//!   the question (`mem supersede <address> <value> --why "..." --answers
//!   ask1`). At dinit that change is a proposal like any other, and the core
//!   reviews it. When the core accepts (or fixes) it, the question closes and
//!   the held change becomes `answered`, linked both ways (`answered_by` on
//!   the held change, `answers` on the answer, `closed.by_change` on the
//!   question). A rejected or held answer is logged on the question, which
//!   stays open for the next person.
//! - The person doesn't know: `mem question dont-know ask1 --why "..."`
//!   records it straight on main and the question stays open.
//! - A human deciding the held change directly (`mem proposal accept ...
//!   --human`) closes the question too.
//! - **Superseded is fixed.** When the fact a question is about gets newer
//!   information on main (an accepted change from anyone, a settled
//!   conflict, a write on main), the question closes as `fixed`, linked to
//!   what fixed it (`p4#1`, `i2`, or the write), and its held change becomes
//!   `fixed` too. Logged on both, in the same commit as the new information.
//!   See [`Memory::close_superseded`].
//!
//! Who may answer is decided in one place, [`Memory::may_answer`]. For now,
//! anyone may.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::memory::{Context, FileWrite, Memory, Provenance};
use crate::proposal::{self, ChangeStatus, Decision, Proposal};
use crate::record;

/// The folder on main that holds questions.
pub const DIR: &str = "questions";
pub const PREFIX: &str = "ask";
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionStatus {
    Open,
    Closed,
}

impl QuestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionStatus::Open => "open",
            QuestionStatus::Closed => "closed",
        }
    }
}

/// Something that happened to a question. Never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub at: String,
    /// `dont-know`, `answer-rejected`, `answer-held`, `answer-accepted`,
    /// `held-change-decided`, or `fixed` (newer information on main).
    pub kind: String,
    /// The shard that asked, or the reviewer.
    pub by: String,
    pub human: Option<String>,
    pub why: String,
    /// The answering change, `p4#1`, when there is one.
    #[serde(default)]
    pub change: Option<String>,
}

/// How a question closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub at: String,
    /// What closed it: the answer (`p4#1`), the held change itself when a
    /// human decided it directly, or, for `fixed`, what put newer
    /// information on main (`p4#1`, an incident `i2`, or a write on main).
    pub by_change: String,
    /// `accept`, `fix`, `reject`, or `fixed`.
    pub action: String,
    /// The reviewer.
    pub by: String,
    pub human: Option<String>,
    pub why: String,
    /// The value that went to main, if any.
    pub value: Option<String>,
}

/// `questions/ask<n>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub format: u32,
    pub id: String,
    /// What to ask the next person.
    pub ask: String,
    pub status: QuestionStatus,
    pub created: String,
    /// The held change: proposal id and change number.
    pub proposal: String,
    pub change: usize,
    /// Fact address, or the `people/` path.
    pub target: String,
    pub path: String,
    /// The value that was proposed and held.
    pub value: Option<String>,
    /// Who said it: the proposing shard and its human.
    pub said_by_shard: String,
    pub said_by_human: Option<String>,
    /// The shard's reason for the change.
    pub said_why: String,
    /// Who held it and why it needs confirming.
    pub held_by: String,
    pub held_why: String,
    /// Everything that happened since, oldest first.
    #[serde(default)]
    pub log: Vec<Answer>,
    #[serde(default)]
    pub closed: Option<Closed>,
}

impl Question {
    pub fn path(&self) -> String {
        path_of(&self.id)
    }
    /// `p3#2`.
    pub fn held_change(&self) -> String {
        format!("{}#{}", self.proposal, self.change)
    }
    pub fn is_open(&self) -> bool {
        self.status == QuestionStatus::Open
    }
}

pub fn path_of(id: &str) -> String {
    format!("{DIR}/{id}.json")
}

fn unknown(id: String) -> Error {
    Error::UnknownQuestion(id)
}

impl Memory {
    /// Who may answer a question. **For now, anyone.** This is the one place
    /// the rule lives: a later rule (people named in the fact's history,
    /// roles, admins) goes here, and every caller (listing questions for a
    /// shard, `--answers`, `dont-know`) follows it.
    pub fn may_answer(&self, q: &Question, human: Option<&str>) -> Result<()> {
        let _ = (q, human);
        Ok(())
    }

    pub(crate) fn question_at(&self, rev: &str, id: &str) -> Result<Question> {
        self.record_at(rev, DIR, PREFIX, id, unknown)
    }

    /// One question, read from main (from any worktree).
    pub fn question(&self, id: &str) -> Result<Question> {
        self.main()?.question_at("HEAD", id)
    }

    /// Every question on main, oldest first.
    pub fn questions(&self) -> Result<Vec<Question>> {
        let main = self.main()?;
        main.record_ids("HEAD", DIR, PREFIX)?
            .iter()
            .map(|id| main.question_at("HEAD", id))
            .collect()
    }

    /// Open questions `human` may answer, oldest first.
    pub fn questions_for(&self, human: Option<&str>) -> Result<Vec<Question>> {
        Ok(self
            .questions()?
            .into_iter()
            .filter(|q| q.is_open() && self.may_answer(q, human).is_ok())
            .collect())
    }

    /// Check an answer before it is written: the question is open, it is
    /// about `target` (a fact address or a repo path), and this person may
    /// answer it.
    pub(crate) fn check_answer(&self, id: &str, target: &str, human: Option<&str>) -> Result<()> {
        let q = self.question(id)?;
        if !q.is_open() {
            return Err(Error::BadQuestion(format!(
                "{id} is already closed ({}); nothing to answer",
                q.closed
                    .as_ref()
                    .map(|c| format!("by {}", c.by_change))
                    .unwrap_or_default()
            )));
        }
        if q.target != target {
            return Err(Error::BadQuestion(format!(
                "{id} asks about {}, not {target}; answer it on {}",
                q.target, q.target
            )));
        }
        self.may_answer(&q, human)
    }

    /// The person doesn't know: logged on the question straight to main,
    /// from any worktree. The question stays open for the next person.
    pub fn question_dont_know(&self, id: &str, prov: &Provenance) -> Result<String> {
        let prov = self.effective(prov, true)?;
        let q = self.question(id)?;
        if !q.is_open() {
            return Err(Error::BadQuestion(format!("{id} is already closed")));
        }
        self.may_answer(&q, prov.human.as_deref())?;
        let main = self.main()?;
        let path = path_of(id);
        main.locked_commit(
            "question-dont-know",
            id,
            std::slice::from_ref(&path),
            &prov,
            &[("Question", id.to_string())],
            |m, prov| {
                let mut q = m.question_at("HEAD", id)?;
                if !q.is_open() {
                    return Err(Error::BadQuestion(format!("{id} is already closed")));
                }
                q.log.push(Answer {
                    at: prov.timestamp(),
                    kind: "dont-know".into(),
                    by: prov.author.clone(),
                    human: prov.human.clone(),
                    why: prov.why.clone(),
                    change: None,
                });
                Ok(vec![(path.clone(), Some(record::render(&q)))])
            },
        )
    }

    /// Newer information on main about `paths` (facts/ or people/ files)
    /// fixes what was waiting on them: every held change on one of those
    /// paths becomes `fixed` (`fixed_by`), and every open question about
    /// one closes as `fixed`, linked to `by`. Called from the same
    /// `prepare` that writes the new information, so it lands in the same
    /// commit. It takes no proposals/questions section lock (that would make
    /// every fact write on main exclusive, whatever the lock scope): it only
    /// rewrites records that already exist, and `prepare` runs under the git
    /// mutex, which every read-modify-write of a record on main also holds.
    /// Records already in `writes` are read from there.
    /// Only on main; a shard's writes aren't on main yet.
    pub(crate) fn close_superseded(
        &self,
        writes: &mut Vec<FileWrite>,
        paths: &[String],
        by: &str,
        prov: &Provenance,
    ) -> Result<()> {
        if self.context != Context::Main {
            return Ok(());
        }
        let paths: Vec<&String> = paths
            .iter()
            .filter(|p| p.starts_with("facts/") || p.starts_with("people/"))
            .collect();
        if paths.is_empty() {
            return Ok(());
        }
        let current = |writes: &Vec<FileWrite>, path: &str| -> Result<Option<String>> {
            match writes.iter().find(|(p, _)| p == path) {
                Some((_, c)) => Ok(c.clone()),
                None => self.read_file_at("HEAD", path),
            }
        };
        let parse = |path: &str, text: &str| -> Result<serde_json::Value> {
            serde_json::from_str(text).map_err(|e| Error::Malformed {
                path: path.to_string(),
                reason: e.to_string(),
            })
        };
        let at = prov.timestamp();
        let note = format!("newer information on main ({by}): {}", prov.why);
        let value_of = |writes: &Vec<FileWrite>, path: &str| -> Result<Option<String>> {
            Ok(
                current(writes, path)?.map(|t| match crate::fact::Fact::parse(&t, path) {
                    Ok(f) if path.starts_with("facts/") => f.value,
                    _ => t,
                }),
            )
        };
        let mut changed: Vec<FileWrite> = Vec::new();
        // Held changes on these paths, and their questions.
        let mut close_qs: Vec<String> = Vec::new();
        for pid in self.record_ids("HEAD", proposal::DIR, "p")? {
            let ppath = proposal::path_of(&pid);
            let Some(text) = current(writes, &ppath)? else {
                continue;
            };
            let mut p: Proposal =
                serde_json::from_value(parse(&ppath, &text)?).map_err(|e| Error::Malformed {
                    path: ppath.clone(),
                    reason: e.to_string(),
                })?;
            let mut touched = false;
            for c in p.changes.iter_mut() {
                if c.status != ChangeStatus::Held || !paths.contains(&&c.path) {
                    continue;
                }
                c.status = ChangeStatus::Fixed;
                c.fixed_by = Some(by.to_string());
                p.log.push(Decision {
                    at: at.clone(),
                    change: c.n,
                    action: "fixed".into(),
                    by: prov.author.clone(),
                    human: prov.human.clone(),
                    why: note.clone(),
                    value: value_of(writes, &c.path)?,
                    record: None,
                    question: c.question.clone(),
                });
                close_qs.extend(c.question.clone());
                touched = true;
            }
            if touched {
                changed.push((ppath, Some(record::render(&p))));
            }
        }
        // Open questions about these paths (their held changes, above).
        for qid in self.record_ids("HEAD", DIR, PREFIX)? {
            let qpath = path_of(&qid);
            let Some(text) = current(writes, &qpath)? else {
                continue;
            };
            let mut q: Question =
                serde_json::from_value(parse(&qpath, &text)?).map_err(|e| Error::Malformed {
                    path: qpath.clone(),
                    reason: e.to_string(),
                })?;
            if !q.is_open() || !(paths.contains(&&q.path) || close_qs.contains(&q.id)) {
                continue;
            }
            q.log.push(Answer {
                at: at.clone(),
                kind: "fixed".into(),
                by: prov.author.clone(),
                human: prov.human.clone(),
                why: note.clone(),
                change: Some(by.to_string()),
            });
            q.status = QuestionStatus::Closed;
            q.closed = Some(Closed {
                at: at.clone(),
                by_change: by.to_string(),
                action: "fixed".into(),
                by: prov.author.clone(),
                human: prov.human.clone(),
                why: note.clone(),
                value: value_of(writes, &q.path)?,
            });
            changed.push((qpath, Some(record::render(&q))));
        }
        for (path, text) in changed {
            match writes.iter_mut().find(|(p, _)| *p == path) {
                Some(w) => w.1 = text,
                None => writes.push((path, text)),
            }
        }
        Ok(())
    }

    /// Open questions as one plain-text block for a shard's first context.
    pub fn questions_brief(&self, human: Option<&str>) -> Result<String> {
        let qs = self.questions_for(human)?;
        if qs.is_empty() {
            return Ok("No open questions.\n".into());
        }
        let mut s = format!(
            "Open questions ({}). Ask one near the start, after the person's own business. \
             Once per question per session; never pester.\n",
            qs.len()
        );
        for q in qs {
            let who = q.said_by_human.as_deref().unwrap_or(&q.said_by_shard);
            s += &format!(
                "\n{}  about {}\n  ask:     {}\n  who said it: {who} (shard {}), proposing {}: \"{}\"\n  why it needs confirming: {}\n",
                q.id,
                q.target,
                q.ask,
                q.said_by_shard,
                q.value.as_deref().unwrap_or("(removal)").replace('\n', " ⏎ "),
                q.said_why,
                q.held_why,
            );
            let dk: Vec<String> = q
                .log
                .iter()
                .filter(|a| a.kind == "dont-know")
                .map(|a| a.human.clone().unwrap_or_else(|| a.by.clone()))
                .collect();
            if !dk.is_empty() {
                s += &format!("  already asked; didn't know: {}\n", dk.join(", "));
            }
            // A rejected answer's reason is often a check's result: the next
            // person should hear it.
            for a in q.log.iter().filter(|a| a.kind == "answer-rejected") {
                s += &format!(
                    "  an earlier answer ({}) was rejected by {}: {}\n",
                    a.change.as_deref().unwrap_or("?"),
                    a.by,
                    a.why
                );
            }
            s += &format!(
                "  if they know:  mem {} {} <value> --why \"<what they said>\" --answers {}\n  if they don't: mem question dont-know {} --why \"<what they said>\"\n",
                if q.path.starts_with("facts/") { "set|supersede" } else { "put" },
                q.target,
                q.id,
                q.id
            );
        }
        Ok(s)
    }
}
