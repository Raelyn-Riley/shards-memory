// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Proposals: a shard's fact and people changes, waiting for the core.
//!
//! The core decides facts and people. So a dinit does not merge a shard's
//! `facts/` and `people/` changes into main: it writes them into one
//! **proposal**, `proposals/p<n>.json` on main, in the dinit's own merge
//! commit. Notes still merge at dinit (they are never facts), the soul still
//! waits for an admin, and quirks never pass through a shard at all.
//!
//! A proposal lists each change (`set`, `supersede`, `put`, or `remove`)
//! with the proposed value, main's value when it was proposed, and the
//! shard's reason. The core decides **per change**:
//!
//! - **accept** writes the value to main, stamped with the shard, its human
//!   and its reason (the fact's `author`, `human`, `why`) plus the reviewer
//!   and the reviewer's reason (`reviewed_by`, `review_why`, `proposal`).
//!   Accepting with `value` set is a **fix**: the edited value goes to main,
//!   and the record keeps both the proposed and the accepted value.
//! - If main changed that path since the proposal was made, accepting does
//!   not write it. It records a **conflict** (`conflicts/<shard>/...`), the
//!   same record a dinit writes, settled with `mem resolve-conflict`.
//! - **reject** leaves main alone. **hold** leaves main alone and waits for
//!   a human: deciding a held change needs a named human. A hold must say
//!   what to ask the next person who can answer (`ask`): it writes an open
//!   question, `questions/ask<n>.json`, in the same commit (see
//!   [`crate::question`]). A change that answers a question (`answers`)
//!   closes it, and marks the held change `answered`, when it is accepted.
//!
//! When a shard changed a path more than once before its dinit, every
//! in-between version and its reason is kept in the change's `drafts`, in
//! order. Drafts were never on main; they are the shard's whole why.
//!
//! Every decision is one commit on main that rewrites the record with the
//! change's new status and one more entry in its `log`. Nothing is removed
//! from a record, and git keeps every earlier version. Nothing is accepted
//! automatically; there is no small-change path.
//!
//! A shard can't decide proposals: deciding is refused from a shard's
//! worktree and for the proposing shard's own id as reviewer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::error::{Error, Result};
use crate::fact::{Fact as FactFile, FrontMatter};
use crate::memory::{now, Context, FileWrite, Memory, Provenance};
use crate::question::{self, Answer, Closed, Question, QuestionStatus};
use crate::record;
use crate::shard::{record_path, target_of, Conflict, Side};

/// The folder on main that holds proposal records.
pub const DIR: &str = "proposals";
/// Bumped on an incompatible change to the record.
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    /// A new fact.
    Set,
    /// A new value for a fact main has.
    Supersede,
    /// A `people/` file written.
    Put,
    /// A file the shard removed.
    Remove,
}

impl ChangeOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeOp::Set => "set",
            ChangeOp::Supersede => "supersede",
            ChangeOp::Put => "put",
            ChangeOp::Remove => "remove",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    /// Waiting for the core.
    Pending,
    /// The core put it aside for a human.
    Held,
    Accepted,
    Rejected,
    /// Main changed the same path first; see `conflict_record`.
    Conflict,
    /// Held, then settled by an accepted answer to its question; see
    /// `answered_by`.
    Answered,
    /// Held, then newer information about the same fact reached main (an
    /// accepted change, a settled conflict, a write on main); see
    /// `fixed_by`.
    Fixed,
}

impl ChangeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeStatus::Pending => "pending",
            ChangeStatus::Held => "held",
            ChangeStatus::Accepted => "accepted",
            ChangeStatus::Rejected => "rejected",
            ChangeStatus::Conflict => "conflict",
            ChangeStatus::Answered => "answered",
            ChangeStatus::Fixed => "fixed",
        }
    }
    /// Still to be decided.
    pub fn is_open(&self) -> bool {
        matches!(self, ChangeStatus::Pending | ChangeStatus::Held)
    }
}

/// One change in a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// 1-based, as given to `--change`.
    pub n: usize,
    pub op: ChangeOp,
    /// Repo path, e.g. `facts/devices/ctl/ip.md` or `people/ada.md`.
    pub path: String,
    /// The fact address for `facts/` paths, else null.
    pub address: Option<String>,
    /// The proposed value (a fact's value, or a whole `people/` file);
    /// null for `remove`.
    pub value: Option<String>,
    /// Main's value when the proposal was made; null if main had none.
    pub main_value: Option<String>,
    /// The shard's reason.
    pub why: String,
    /// Who made the change on the shard (the shard id).
    pub author: String,
    /// When the shard made it.
    pub updated: String,
    pub status: ChangeStatus,
    /// The value written to main, once accepted. Differs from `value` when
    /// the reviewer fixed it.
    #[serde(default)]
    pub accepted_value: Option<String>,
    /// The conflict record, when accepting found main had moved.
    #[serde(default)]
    pub conflict_record: Option<String>,
    /// The shard's earlier versions of this path before the dinit, oldest
    /// first. Never on main.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drafts: Vec<Draft>,
    /// The open question this change answers (`ask2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<String>,
    /// For a held change: the question asked about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// For an answered change: the accepted answer, `p4#1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    /// For a fixed change: what put newer information on main (`p4#1`, an
    /// incident `i2`, or a write on main).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_by: Option<String>,
}

/// One of the shard's in-between versions of a path, before its dinit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    /// The shard's commit.
    pub commit: String,
    /// The value then; null if the shard had removed it.
    pub value: Option<String>,
    pub why: String,
    pub author: String,
    pub updated: String,
}

impl Change {
    /// Address or path: what a person calls it.
    pub fn target(&self) -> &str {
        self.address.as_deref().unwrap_or(&self.path)
    }
}

/// One decision, as logged in the record. Never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub at: String,
    pub change: usize,
    /// `accept`, `fix` (accept with an edited value), `reject`, `hold`, or
    /// `conflict` (an accept that found main had moved).
    pub action: String,
    /// The reviewer (the core's author id, or whoever decided).
    pub by: String,
    pub human: Option<String>,
    /// The reviewer's reason.
    pub why: String,
    /// For `fix` and `conflict`: the value the reviewer accepted.
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub record: Option<String>,
    /// For `hold`: the question it asked. For an accepted or rejected
    /// answer: the question it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
}

/// `proposals/p<n>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub format: u32,
    pub id: String,
    /// The proposing shard.
    pub shard: String,
    /// The shard's human, if known.
    pub human: Option<String>,
    /// When the dinit made it.
    pub created: String,
    /// Main at the dinit: each change is checked against main's file here
    /// when it is accepted.
    pub main_commit: String,
    /// The shard's tip at the dinit (an ancestor of main from then on).
    pub shard_commit: String,
    pub changes: Vec<Change>,
    /// Every decision, oldest first.
    pub log: Vec<Decision>,
}

impl Proposal {
    pub fn path(&self) -> String {
        path_of(&self.id)
    }
    pub fn count(&self, s: ChangeStatus) -> usize {
        self.changes.iter().filter(|c| c.status == s).count()
    }
    /// Has a change still to decide (pending or held).
    pub fn is_open(&self) -> bool {
        self.changes.iter().any(|c| c.status.is_open())
    }
    fn render(&self) -> String {
        record::render(self)
    }
    /// The change numbered `n`.
    pub fn change(&self, n: usize) -> Option<&Change> {
        self.changes.iter().find(|c| c.n == n)
    }
}

/// What the reviewer decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Accept; `Some(value)` accepts an edited value instead (a fix).
    Accept(Option<String>),
    Reject,
    /// Hold for a human, asking this of the next person who can answer.
    Hold(String),
}

impl Verdict {
    fn op(&self) -> &'static str {
        match self {
            Verdict::Accept(_) => "proposal-accept",
            Verdict::Reject => "proposal-reject",
            Verdict::Hold(_) => "proposal-hold",
        }
    }
}

/// What one decision did to each change it covered.
#[derive(Debug, Clone)]
pub struct DecideReport {
    pub id: String,
    pub commit: String,
    /// (change number, target, new status, conflict record).
    pub changes: Vec<(usize, String, ChangeStatus, Option<String>)>,
}

pub fn path_of(id: &str) -> String {
    format!("{DIR}/{id}.json")
}

impl Memory {
    fn proposal_at(&self, rev: &str, id: &str) -> Result<Proposal> {
        self.record_at(rev, DIR, "p", id, Error::UnknownProposal)
    }

    /// One proposal, read from main (from any worktree).
    pub fn proposal(&self, id: &str) -> Result<Proposal> {
        self.proposal_at("main", id)
    }

    /// Every proposal on main, oldest first.
    pub fn proposals(&self) -> Result<Vec<Proposal>> {
        self.record_ids("main", DIR, "p")?
            .iter()
            .map(|id| self.proposal_at("main", id))
            .collect()
    }

    /// Build the proposal for a dinit: `paths` (facts/ and people/) changed
    /// on the shard at `tip`, not changed on main since the base. Called
    /// under the dinit's locks.
    pub(crate) fn build_proposal(
        &self,
        shard: &str,
        human: Option<&str>,
        main_head: &str,
        base: &str,
        tip: &str,
        paths: &[String],
    ) -> Result<Proposal> {
        let id = self.next_record_id(main_head, DIR, "p")?;
        let mut changes = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let address = Address::from_home(p).map(|a| a.to_string());
            let on_main = self.read_file_at(main_head, p)?.is_some();
            let on_shard = self.read_file_at(tip, p)?.is_some();
            let op = match (on_shard, address.is_some(), on_main) {
                (false, _, _) => ChangeOp::Remove,
                (true, true, false) => ChangeOp::Set,
                (true, true, true) => ChangeOp::Supersede,
                (true, false, _) => ChangeOp::Put,
            };
            let side = self.side(tip, p)?;
            let (drafts, answers) = self.drafts_of(base, tip, p)?;
            changes.push(Change {
                n: i + 1,
                op,
                path: p.clone(),
                address,
                value: on_shard.then(|| side.value.clone()),
                main_value: if on_main {
                    Some(self.side(main_head, p)?.value)
                } else {
                    None
                },
                why: side.why,
                author: side.author,
                updated: side.updated,
                status: ChangeStatus::Pending,
                accepted_value: None,
                conflict_record: None,
                drafts,
                answers,
                question: None,
                answered_by: None,
                fixed_by: None,
            });
        }
        Ok(Proposal {
            format: FORMAT,
            id,
            shard: shard.to_string(),
            human: human.map(String::from),
            created: now(),
            main_commit: main_head.to_string(),
            shard_commit: tip.to_string(),
            changes,
            log: Vec::new(),
        })
    }

    /// The shard's versions of `path` from `base` to `tip`, oldest first:
    /// every one but the last (the proposed value) is a draft. Also the
    /// latest question any of them cited with `--answers`.
    fn drafts_of(&self, base: &str, tip: &str, path: &str) -> Result<(Vec<Draft>, Option<String>)> {
        let out = self.git.run(&[
            "log",
            "--reverse",
            "--topo-order",
            "--format=%H%x00%(trailers:key=Shards-Answers,valueonly,separator=)",
            &format!("{base}..{tip}"),
            "--",
            path,
        ])?;
        let mut versions = Vec::new();
        let mut answers = None;
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let (commit, ans) = line.split_once('\0').unwrap_or((line, ""));
            if !ans.trim().is_empty() {
                answers = Some(ans.trim().to_string());
            }
            let text = self.read_file_at(commit, path)?;
            let side = self.side(commit, path)?;
            versions.push(Draft {
                commit: commit.to_string(),
                value: text.map(|_| side.value.clone()),
                why: side.why,
                author: side.author,
                updated: side.updated,
            });
        }
        versions.pop();
        Ok((versions, answers))
    }

    /// Every change on main that drafted `target` (a fact address or a
    /// repo path), oldest proposal first: `(proposal, change)`.
    pub fn drafts_for(&self, target: &str) -> Result<Vec<(Proposal, Change)>> {
        let mut out = Vec::new();
        for p in self.main()?.proposals()? {
            for c in &p.changes {
                if c.target() == target && !c.drafts.is_empty() {
                    out.push((p.clone(), c.clone()));
                }
            }
        }
        Ok(out)
    }

    /// The text of a proposal record, for the dinit's merge tree.
    pub(crate) fn proposal_text(p: &Proposal) -> String {
        p.render()
    }

    /// Decide on a proposal: every open change, or only change `change`.
    /// One commit on main. See the module docs.
    pub fn proposal_decide(
        &self,
        id: &str,
        change: Option<usize>,
        verdict: &Verdict,
        prov: &Provenance,
    ) -> Result<DecideReport> {
        if let Context::Shard(s) = &self.context {
            return Err(Error::Proposal(format!(
                "shard `{s}` can't decide proposals; the core reviews them on main"
            )));
        }
        let main = self.main()?;
        let mut prov = prov.clone();
        if prov.author.trim().is_empty() {
            prov.author = "core".to_string();
        }
        let prov = main.effective(&prov, true)?;
        match verdict {
            Verdict::Accept(Some(v)) => {
                if change.is_none() {
                    return Err(Error::Proposal(
                        "an edited value fixes one change: pass --change N with --value".into(),
                    ));
                }
                if v.trim().is_empty() {
                    return Err(Error::EmptyValue);
                }
            }
            Verdict::Hold(ask) => {
                if ask.trim().is_empty() {
                    return Err(Error::Proposal(
                        "a hold needs --ask \"<question>\": what to ask the next person who can \
                         answer it, e.g. --ask \"Mike mentioned he replaced the spindle, but only \
                         in passing. Would you confirm it so the record knows for sure?\""
                            .into(),
                    ));
                }
                if ask.contains('\n') || ask.contains('\r') {
                    return Err(Error::Multiline { field: "ask" });
                }
            }
            _ => {}
        }
        // Lock what this decision may write, from the record as it stands;
        // everything is checked again under the lock.
        let before = main.proposal(id)?;
        if prov.author == before.shard {
            return Err(Error::Proposal(format!(
                "`{}` proposed {id}; a shard can't review its own proposal",
                before.shard
            )));
        }
        let mut lock_paths: Vec<String> = before.changes.iter().map(|c| c.path.clone()).collect();
        lock_paths.push(before.path());
        lock_paths.push(format!("conflicts/{}/x", before.shard));
        lock_paths.push(format!("{}/_", question::DIR));
        for c in &before.changes {
            for q in c.answers.iter().chain(c.question.iter()) {
                lock_paths.push(question::path_of(q));
                if let Ok(q) = main.question(q) {
                    lock_paths.push(path_of(&q.proposal));
                }
            }
        }

        let mut outcome = Vec::new();
        let mut trailers = vec![
            ("Proposal", id.to_string()),
            ("Shard", before.shard.clone()),
        ];
        if let Some(h) = &before.human {
            trailers.push(("Shard-Human", h.clone()));
        }
        for c in &before.changes {
            if change.map_or(c.status.is_open(), |n| n == c.n) {
                trailers.push(("Change", format!("{} {}", c.n, c.target())));
                trailers.push(("Shard-Why", format!("{}: {}", c.n, c.why)));
            }
        }
        if let Verdict::Hold(ask) = verdict {
            trailers.push(("Ask", ask.clone()));
        }
        let commit = main.locked_commit(
            verdict.op(),
            id,
            &lock_paths,
            &prov,
            &trailers,
            |m, prov| m.decide_writes(id, change, verdict, prov, &mut outcome),
        )?;
        Ok(DecideReport {
            id: id.to_string(),
            commit,
            changes: outcome,
        })
    }

    /// Under the lock: the record's new state and every file to write.
    fn decide_writes(
        &self,
        id: &str,
        change: Option<usize>,
        verdict: &Verdict,
        prov: &Provenance,
        outcome: &mut Vec<(usize, String, ChangeStatus, Option<String>)>,
    ) -> Result<Vec<FileWrite>> {
        let mut p = self.proposal_at("HEAD", id)?;
        let at = prov.timestamp();
        let targets: Vec<usize> = match change {
            Some(n) => {
                let c = p.changes.iter().find(|c| c.n == n).ok_or_else(|| {
                    Error::Proposal(format!(
                        "{id} has no change {n} (it has {})",
                        p.changes.len()
                    ))
                })?;
                if !c.status.is_open() {
                    return Err(Error::Proposal(format!(
                        "change {n} of {id} is already {}; a decision is final",
                        c.status.as_str()
                    )));
                }
                vec![n]
            }
            None => p
                .changes
                .iter()
                .filter(|c| c.status.is_open())
                .map(|c| c.n)
                .collect(),
        };
        if targets.is_empty() {
            return Err(Error::Proposal(format!("{id} has nothing left to decide")));
        }
        let mut writes: Vec<FileWrite> = Vec::new();
        let mut seen_records = BTreeSet::new();
        // Questions and other proposals this decision touches.
        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        let mut others: BTreeMap<String, Proposal> = BTreeMap::new();
        let mut next_q = record::num(
            question::PREFIX,
            &self.next_record_id("HEAD", question::DIR, question::PREFIX)?,
        );
        for n in targets {
            let c = p.changes.iter_mut().find(|c| c.n == n).expect("listed");
            if c.status == ChangeStatus::Held && prov.human.is_none() {
                return Err(Error::Proposal(format!(
                    "change {n} of {id} is held for a human: pass --human <name> to decide it"
                )));
            }
            let was_held = c.status == ChangeStatus::Held;
            let mut d = Decision {
                at: at.clone(),
                change: n,
                action: String::new(),
                by: prov.author.clone(),
                human: prov.human.clone(),
                why: prov.why.clone(),
                value: None,
                record: None,
                question: None,
            };
            match verdict {
                Verdict::Reject => {
                    c.status = ChangeStatus::Rejected;
                    d.action = "reject".into();
                }
                Verdict::Hold(ask) => {
                    if c.status == ChangeStatus::Held {
                        return Err(Error::Proposal(format!(
                            "change {n} of {id} is already held"
                        )));
                    }
                    let qid = format!("{}{next_q}", question::PREFIX);
                    next_q += 1;
                    questions.insert(
                        qid.clone(),
                        Question {
                            format: question::FORMAT,
                            id: qid.clone(),
                            ask: ask.trim().to_string(),
                            status: QuestionStatus::Open,
                            created: at.clone(),
                            proposal: id.to_string(),
                            change: n,
                            target: c.target().to_string(),
                            path: c.path.clone(),
                            value: c.value.clone(),
                            said_by_shard: p.shard.clone(),
                            said_by_human: p.human.clone(),
                            said_why: c.why.clone(),
                            held_by: match &prov.human {
                                Some(h) => format!("{} / {h}", prov.author),
                                None => prov.author.clone(),
                            },
                            held_why: prov.why.clone(),
                            log: Vec::new(),
                            closed: None,
                        },
                    );
                    c.status = ChangeStatus::Held;
                    c.question = Some(qid.clone());
                    d.action = "hold".into();
                    d.question = Some(qid);
                }
                Verdict::Accept(edit) => {
                    if edit.is_some() && c.op == ChangeOp::Remove {
                        return Err(Error::Proposal(format!(
                            "change {n} of {id} removes {}; there is no value to fix",
                            c.target()
                        )));
                    }
                    let value = edit.clone().or_else(|| c.value.clone());
                    let then = self.read_file_at(&p.main_commit, &c.path)?;
                    let current = self.read_file_at("HEAD", &c.path)?;
                    if then != current {
                        // Main moved on this path: a conflict, as at dinit.
                        let record = record_path(&p.shard, &c.path);
                        let conflict = Conflict {
                            target: target_of(&c.path),
                            path: c.path.clone(),
                            record: record.clone(),
                            main: self.side("HEAD", &c.path)?,
                            shard: Side {
                                value: value.clone().unwrap_or_else(|| "(deleted)".into()),
                                author: c.author.clone(),
                                human: p.human.clone(),
                                updated: c.updated.clone(),
                                why: c.why.clone(),
                            },
                        };
                        let head = self.git.run(&["rev-parse", "HEAD"])?.trim().to_string();
                        let text = self.render_record(&p.shard, &conflict, &head, &p.shard_commit);
                        if seen_records.insert(record.clone()) {
                            writes.push((record.clone(), Some(text)));
                        }
                        c.status = ChangeStatus::Conflict;
                        c.conflict_record = Some(record.clone());
                        d.action = "conflict".into();
                        d.value = value;
                        d.record = Some(record);
                    } else {
                        let contents = match (&value, Address::from_home(&c.path)) {
                            (None, _) => None,
                            (Some(v), Some(a)) => Some(
                                FactFile {
                                    front: FrontMatter {
                                        address: a.to_string(),
                                        updated: at.clone(),
                                        author: c.author.clone(),
                                        human: p.human.clone().filter(|h| !h.is_empty()),
                                        why: c.why.clone(),
                                        proposal: Some(format!("{id}#{n}")),
                                        reviewed_by: Some(match &prov.human {
                                            Some(h) => format!("{} / {h}", prov.author),
                                            None => prov.author.clone(),
                                        }),
                                        review_why: Some(prov.why.clone()),
                                    },
                                    value: v.clone(),
                                }
                                .render(),
                            ),
                            (Some(v), None) => Some(v.clone()),
                        };
                        writes.push((c.path.clone(), contents));
                        c.status = ChangeStatus::Accepted;
                        c.accepted_value = value.clone();
                        if edit.is_some() {
                            d.action = "fix".into();
                            d.value = value;
                        } else {
                            d.action = "accept".into();
                        }
                    }
                }
            }
            let this = format!("{id}#{n}");
            let decided = matches!(d.action.as_str(), "accept" | "fix" | "reject");
            // A held change decided directly by a human closes its question.
            if let (true, true, Some(q)) = (was_held, decided, c.question.clone()) {
                let q = load_question(self, &mut questions, &q)?;
                if q.is_open() {
                    q.log.push(Answer {
                        at: at.clone(),
                        kind: "held-change-decided".into(),
                        by: prov.author.clone(),
                        human: prov.human.clone(),
                        why: prov.why.clone(),
                        change: Some(this.clone()),
                    });
                    q.status = QuestionStatus::Closed;
                    q.closed = Some(Closed {
                        at: at.clone(),
                        by_change: this.clone(),
                        action: d.action.clone(),
                        by: prov.author.clone(),
                        human: prov.human.clone(),
                        why: prov.why.clone(),
                        value: c.accepted_value.clone(),
                    });
                }
            }
            // An answer to an open question.
            if let Some(qid) = c.answers.clone() {
                let q = load_question(self, &mut questions, &qid)?;
                if q.is_open() {
                    d.question = Some(qid.clone());
                    let kind = match d.action.as_str() {
                        "accept" | "fix" => "answer-accepted",
                        "reject" => "answer-rejected",
                        "hold" => "answer-held",
                        _ => "answer-conflict",
                    };
                    q.log.push(Answer {
                        at: at.clone(),
                        kind: kind.into(),
                        by: prov.author.clone(),
                        human: prov.human.clone(),
                        why: prov.why.clone(),
                        change: Some(this.clone()),
                    });
                    if kind == "answer-accepted" {
                        q.status = QuestionStatus::Closed;
                        q.closed = Some(Closed {
                            at: at.clone(),
                            by_change: this.clone(),
                            action: d.action.clone(),
                            by: prov.author.clone(),
                            human: prov.human.clone(),
                            why: prov.why.clone(),
                            value: c.accepted_value.clone(),
                        });
                        let (held_p, held_n) = (q.proposal.clone(), q.change);
                        let note = format!("{qid} answered by {this}: {}", prov.why);
                        let mark = |hp: &mut Proposal| {
                            if let Some(h) = hp.changes.iter_mut().find(|h| h.n == held_n) {
                                if h.status == ChangeStatus::Held {
                                    h.status = ChangeStatus::Answered;
                                    h.answered_by = Some(this.clone());
                                    hp.log.push(Decision {
                                        at: at.clone(),
                                        change: held_n,
                                        action: "answered".into(),
                                        by: prov.author.clone(),
                                        human: prov.human.clone(),
                                        why: note.clone(),
                                        value: c.accepted_value.clone(),
                                        record: None,
                                        question: Some(qid.clone()),
                                    });
                                }
                            }
                        };
                        // The question was asked after this proposal's held
                        // change, so an answer is always in a later proposal.
                        if held_p != id {
                            if !others.contains_key(&held_p) {
                                others.insert(held_p.clone(), self.proposal_at("HEAD", &held_p)?);
                            }
                            mark(others.get_mut(&held_p).expect("loaded"));
                        }
                    }
                }
            }
            outcome.push((
                n,
                c.target().to_string(),
                c.status,
                c.conflict_record.clone(),
            ));
            p.log.push(d);
        }
        let fixes: Vec<(String, String)> = p
            .changes
            .iter()
            .filter(|c| c.status == ChangeStatus::Accepted && outcome.iter().any(|o| o.0 == c.n))
            .map(|c| (c.path.clone(), format!("{id}#{}", c.n)))
            .collect();
        writes.push((p.path(), Some(p.render())));
        for o in others.into_values() {
            writes.push((o.path(), Some(o.render())));
        }
        for q in questions.values() {
            writes.push((q.path(), Some(record::render(q))));
        }
        // An accepted change is newer information about its fact: anything
        // else held on it is fixed, and its open questions close.
        for (path, by) in fixes {
            self.close_superseded(&mut writes, &[path], &by, prov)?;
        }
        Ok(writes)
    }
}

/// A question this decision touches, loaded once from HEAD.
fn load_question<'a>(
    m: &Memory,
    qs: &'a mut BTreeMap<String, Question>,
    id: &str,
) -> Result<&'a mut Question> {
    if !qs.contains_key(id) {
        qs.insert(id.to_string(), m.question_at("HEAD", id)?);
    }
    Ok(qs.get_mut(id).expect("loaded"))
}
