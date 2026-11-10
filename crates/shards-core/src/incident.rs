// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Incidents: the record of a conflict a person settled.
//!
//! A person's pick in a conflict is final: `mem resolve-conflict` puts the
//! chosen value on main with no proposal review, because a person settling
//! it outranks the core. Every resolution also writes an **incident**,
//! `incidents/i<n>.json` on main, in the same commit. It holds a one-line
//! summary for people ("two shards conflicted over kiln/east/temp. Ada
//! checked it. shard s2 was correct."), both sides' values and reasons,
//! which shards and humans were involved, who settled it, their reason, and
//! when.
//!
//! A new incident wakes the core (the `incident` trigger). The core may add
//! its own note with a reason (`mem incident note`). Notes are appended;
//! nothing in an incident is ever changed or deleted, and git keeps every
//! version of the record.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::memory::{Context, Memory, Provenance};
use crate::record;

/// The folder on main that holds incidents.
pub const DIR: &str = "incidents";
pub const PREFIX: &str = "i";
pub const FORMAT: u32 = 1;
/// How many incidents the board and the core's context list.
pub const RECENT: usize = 5;

/// One side of the conflict, as it stood when the conflict was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentSide {
    pub value: String,
    /// Who wrote it (a shard id, or the core).
    pub author: String,
    pub human: Option<String>,
    pub updated: String,
    pub why: String,
}

/// How the person settled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    /// `shard`, `main` or `value`.
    pub take: String,
    /// The value on main afterwards.
    pub value: String,
    /// Which side was right: `shard <id>`, `main`, or `neither`.
    pub correct: String,
    /// The person who settled it.
    pub human: String,
    /// Who ran it (the session, or the person).
    pub by: String,
    pub why: String,
    pub at: String,
}

/// A note added after the fact, usually by the core. Never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentNote {
    pub at: String,
    pub by: String,
    pub human: Option<String>,
    pub why: String,
    pub note: String,
}

/// `incidents/i<n>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incident {
    pub format: u32,
    pub id: String,
    /// One line for people.
    pub summary: String,
    /// Fact address, or the repo path for people and notes.
    pub target: String,
    pub path: String,
    /// The conflict record this settled (removed from main by the settle).
    pub conflict_record: String,
    /// When the conflict was recorded.
    pub conflict_recorded: String,
    /// The shard whose change conflicted with main.
    pub shard: String,
    /// Main's side.
    pub main: IncidentSide,
    /// The shard's side.
    pub shard_side: IncidentSide,
    /// Every shard involved (main's author first when it is a shard).
    pub shards: Vec<String>,
    /// Every human involved: both sides and the settler.
    pub humans: Vec<String>,
    pub settled: Settlement,
    /// Notes added later, oldest first.
    #[serde(default)]
    pub notes: Vec<IncidentNote>,
}

impl Incident {
    pub fn path(&self) -> String {
        path_of(&self.id)
    }
}

pub fn path_of(id: &str) -> String {
    format!("{DIR}/{id}.json")
}

/// The one-line summary.
pub(crate) fn summary(target: &str, shard: &str, main_author: &str, s: &Settlement) -> String {
    let head = format!(
        "two shards conflicted over {target}. {} checked it.",
        s.human
    );
    match s.take.as_str() {
        "shard" => format!("{head} shard {shard} was correct."),
        "main" if !main_author.is_empty() => format!("{head} shard {main_author} was correct."),
        "main" => format!("{head} main was correct."),
        _ => format!(
            "{head} neither was correct; it is now {}.",
            s.value.replace('\n', " ⏎ ")
        ),
    }
}

fn unknown(id: String) -> Error {
    Error::UnknownIncident(id)
}

impl Memory {
    /// One incident, read from main (from any worktree).
    pub fn incident(&self, id: &str) -> Result<Incident> {
        self.main()?.record_at("HEAD", DIR, PREFIX, id, unknown)
    }

    /// Every incident on main, oldest first.
    pub fn incidents(&self) -> Result<Vec<Incident>> {
        let main = self.main()?;
        main.record_ids("HEAD", DIR, PREFIX)?
            .iter()
            .map(|id| main.record_at("HEAD", DIR, PREFIX, id, unknown))
            .collect()
    }

    /// The newest `RECENT` incidents, newest first.
    pub fn recent_incidents(&self) -> Result<Vec<Incident>> {
        let mut all = self.incidents()?;
        all.reverse();
        all.truncate(RECENT);
        Ok(all)
    }

    /// Add a note to an incident, with a reason. One commit on main. The
    /// note is appended; nothing already in the record changes. Not from a
    /// shard: it is the core's (or a person's at main) note.
    pub fn incident_note(&self, id: &str, note: &str, prov: &Provenance) -> Result<String> {
        if let Context::Shard(s) = &self.context {
            return Err(Error::BadIncident(format!(
                "shard `{s}` can't add notes to incidents; the core does, on main"
            )));
        }
        if note.trim().is_empty() {
            return Err(Error::BadIncident("the note is empty".into()));
        }
        let main = self.main()?;
        let mut prov = prov.clone();
        if prov.author.trim().is_empty() {
            prov.author = "core".to_string();
        }
        let prov = main.effective(&prov, true)?;
        // Checked before the lock for a clear error; read again under it.
        main.incident(id)?;
        let path = path_of(id);
        main.locked_commit(
            "incident-note",
            id,
            std::slice::from_ref(&path),
            &prov,
            &[("Incident", id.to_string())],
            |m, prov| {
                let mut i: Incident = m.record_at("HEAD", DIR, PREFIX, id, unknown)?;
                i.notes.push(IncidentNote {
                    at: prov.timestamp(),
                    by: prov.author.clone(),
                    human: prov.human.clone(),
                    why: prov.why.clone(),
                    note: note.trim_end().to_string(),
                });
                Ok(vec![(path.clone(), Some(record::render(&i)))])
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(take: &str, value: &str) -> Settlement {
        Settlement {
            take: take.into(),
            value: value.into(),
            correct: String::new(),
            human: "Ada".into(),
            by: "core".into(),
            why: "checked the dial".into(),
            at: "2026-11-10T10:00:00Z".into(),
        }
    }

    #[test]
    fn summaries() {
        assert_eq!(
            summary("kiln/east/temp", "s2", "s1", &s("shard", "910")),
            "two shards conflicted over kiln/east/temp. Ada checked it. shard s2 was correct."
        );
        assert_eq!(
            summary("kiln/east/temp", "s2", "s1", &s("main", "900")),
            "two shards conflicted over kiln/east/temp. Ada checked it. shard s1 was correct."
        );
        assert!(summary("kiln/east/temp", "s2", "s1", &s("value", "905"))
            .ends_with("neither was correct; it is now 905."));
    }
}
