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
//!   a human: deciding a held change needs a named human.
//!
//! Every decision is one commit on main that rewrites the record with the
//! change's new status and one more entry in its `log`. Nothing is removed
//! from a record, and git keeps every earlier version. Nothing is accepted
//! automatically; there is no small-change path.
//!
//! A shard can't decide proposals: deciding is refused from a shard's
//! worktree and for the proposing shard's own id as reviewer.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::error::{Error, Result};
use crate::fact::{Fact as FactFile, FrontMatter};
use crate::memory::{now, Context, FileWrite, Memory, Provenance};
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
}

impl ChangeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeStatus::Pending => "pending",
            ChangeStatus::Held => "held",
            ChangeStatus::Accepted => "accepted",
            ChangeStatus::Rejected => "rejected",
            ChangeStatus::Conflict => "conflict",
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
        let mut s = serde_json::to_string_pretty(self).expect("serializable");
        s.push('\n');
        s
    }
}

/// What the reviewer decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Accept; `Some(value)` accepts an edited value instead (a fix).
    Accept(Option<String>),
    Reject,
    Hold,
}

impl Verdict {
    fn op(&self) -> &'static str {
        match self {
            Verdict::Accept(_) => "proposal-accept",
            Verdict::Reject => "proposal-reject",
            Verdict::Hold => "proposal-hold",
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

fn valid_id(id: &str) -> bool {
    id.strip_prefix('p')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn num(id: &str) -> u64 {
    id.trim_start_matches('p').parse().unwrap_or(0)
}

impl Memory {
    /// Proposal ids on `rev`, by number.
    fn proposal_ids(&self, rev: &str) -> Result<Vec<String>> {
        let out = self.git.run(&[
            "ls-tree",
            "-r",
            "--name-only",
            rev,
            "--",
            &format!("{DIR}/"),
        ])?;
        let mut ids: Vec<String> = out
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("{DIR}/"))?.strip_suffix(".json"))
            .filter(|id| valid_id(id))
            .map(String::from)
            .collect();
        ids.sort_by_key(|id| num(id));
        Ok(ids)
    }

    fn proposal_at(&self, rev: &str, id: &str) -> Result<Proposal> {
        if !valid_id(id) {
            return Err(Error::UnknownProposal(id.to_string()));
        }
        let path = path_of(id);
        let text = self
            .read_file_at(rev, &path)?
            .ok_or_else(|| Error::UnknownProposal(id.to_string()))?;
        serde_json::from_str(&text).map_err(|e| Error::Malformed {
            path,
            reason: e.to_string(),
        })
    }

    /// One proposal, read from main (from any worktree).
    pub fn proposal(&self, id: &str) -> Result<Proposal> {
        self.proposal_at("main", id)
    }

    /// Every proposal on main, oldest first.
    pub fn proposals(&self) -> Result<Vec<Proposal>> {
        self.proposal_ids("main")?
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
        tip: &str,
        paths: &[String],
    ) -> Result<Proposal> {
        let next = self
            .proposal_ids(main_head)?
            .last()
            .map(|id| num(id))
            .unwrap_or(0)
            + 1;
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
            });
        }
        Ok(Proposal {
            format: FORMAT,
            id: format!("p{next}"),
            shard: shard.to_string(),
            human: human.map(String::from),
            created: now(),
            main_commit: main_head.to_string(),
            shard_commit: tip.to_string(),
            changes,
            log: Vec::new(),
        })
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
        if let Verdict::Accept(Some(v)) = verdict {
            if change.is_none() {
                return Err(Error::Proposal(
                    "an edited value fixes one change: pass --change N with --value".into(),
                ));
            }
            if v.trim().is_empty() {
                return Err(Error::EmptyValue);
            }
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
        for n in targets {
            let c = p.changes.iter_mut().find(|c| c.n == n).expect("listed");
            if c.status == ChangeStatus::Held && prov.human.is_none() {
                return Err(Error::Proposal(format!(
                    "change {n} of {id} is held for a human: pass --human <name> to decide it"
                )));
            }
            let mut d = Decision {
                at: at.clone(),
                change: n,
                action: String::new(),
                by: prov.author.clone(),
                human: prov.human.clone(),
                why: prov.why.clone(),
                value: None,
                record: None,
            };
            match verdict {
                Verdict::Reject => {
                    c.status = ChangeStatus::Rejected;
                    d.action = "reject".into();
                }
                Verdict::Hold => {
                    if c.status == ChangeStatus::Held {
                        return Err(Error::Proposal(format!(
                            "change {n} of {id} is already held"
                        )));
                    }
                    c.status = ChangeStatus::Held;
                    d.action = "hold".into();
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
            outcome.push((
                n,
                c.target().to_string(),
                c.status,
                c.conflict_record.clone(),
            ));
            p.log.push(d);
        }
        writes.push((p.path(), Some(p.render())));
        Ok(writes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(valid_id("p1") && valid_id("p12"));
        assert!(!valid_id("p") && !valid_id("q1") && !valid_id("p1/../x"));
        assert_eq!(num("p12"), 12);
    }
}
