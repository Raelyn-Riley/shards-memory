// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Notes with owners.
//!
//! - `notes/<file>.md`, at the top of `notes/`, is a **shared** note.
//! - `notes/<person>/...` is that person's own note. The folder name is the
//!   person.
//!
//! Notes are plain markdown, never facts: nothing here reads or writes
//! `facts/`, and a note's text is never taken as a fact's value. They are
//! written like any other `notes/` file, so in a shard they ride its branch
//! and merge at dinit like before.

use std::cmp::Ordering;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::memory::{Memory, Provenance};

pub const DIR: &str = "notes";

/// Whose note it is, from its path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "person", rename_all = "lowercase")]
pub enum Owner {
    Shared,
    Person(String),
}

impl Owner {
    /// `notes/x.md` is shared; `notes/<person>/...` is the person's.
    /// Anything outside `notes/` is not a note.
    pub fn of(path: &str) -> Option<Owner> {
        let rest = path.strip_prefix("notes/")?;
        match rest.split_once('/') {
            None if !rest.is_empty() => Some(Owner::Shared),
            Some((person, file)) if !person.is_empty() && !file.is_empty() => {
                Some(Owner::Person(person.to_string()))
            }
            _ => None,
        }
    }
}

/// Where a note found by [`Memory::find_notes`] ranks for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rank {
    /// The person's own note.
    Own,
    Shared,
    /// Someone else's own note.
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteHit {
    pub path: String,
    pub owner: Owner,
    pub rank: Rank,
    /// When the note was last written: the author date of the newest commit
    /// that changed it (RFC 3339).
    pub date: String,
    /// The lines that contain the word (at most three).
    pub lines: Vec<String>,
}

impl NoteHit {
    /// The date part of [`NoteHit::date`], `YYYY-MM-DD`.
    pub fn day(&self) -> &str {
        self.date.get(..10).unwrap_or(&self.date)
    }
}

fn check_person(person: &str) -> Result<()> {
    let bad = person.trim().is_empty()
        || person != person.trim()
        || person.contains('/')
        || person.contains('\\')
        || person.starts_with('.');
    if bad {
        return Err(Error::BadPath {
            path: format!("notes/{person}/"),
            reason: "a person is one folder name: not empty, no `/`, not starting with `.`"
                .to_string(),
        });
    }
    Ok(())
}

/// The path of `path` in `person`'s folder. `path` may be given as
/// `notes/<file>.md`, `<file>.md`, or already as `notes/<person>/<file>.md`.
pub fn path_for(person: &str, path: &str) -> Result<String> {
    check_person(person)?;
    let top = path.split('/').next().unwrap_or("");
    if ["facts", "people", "soul"].contains(&top) {
        return Err(Error::BadPath {
            path: path.to_string(),
            reason: "a person's note goes under notes/<person>/".to_string(),
        });
    }
    let rest = path.strip_prefix("notes/").unwrap_or(path);
    let mine = format!("{person}/");
    if rest.starts_with(&mine) {
        Ok(format!("notes/{rest}"))
    } else {
        Ok(format!("notes/{person}/{rest}"))
    }
}

impl Memory {
    /// Write a note: into `person`'s folder when given, else at `path` as
    /// before (a shared note at the top of `notes/`). Returns the path
    /// written and the commit.
    pub fn put_note_as(
        &self,
        person: Option<&str>,
        path: &str,
        text: &str,
        prov: &Provenance,
    ) -> Result<(String, String)> {
        let path = match person {
            Some(p) => path_for(p, path)?,
            None => path.to_string(),
        };
        let commit = self.put_file(&path, text, prov)?;
        Ok((path, commit))
    }

    /// Every note on HEAD that contains `word` (case-insensitive, in its
    /// text or its path): `person`'s own first, then the shared notes, then
    /// everyone else's; newest first within each.
    pub fn find_notes(&self, word: &str, person: &str) -> Result<Vec<NoteHit>> {
        check_person(person)?;
        let needle = word.trim().to_lowercase();
        if needle.is_empty() {
            return Err(Error::BadPath {
                path: word.to_string(),
                reason: "give a word to find".to_string(),
            });
        }
        let list = self.git.run(&[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            "HEAD",
            "--",
            &format!("{DIR}/"),
        ])?;
        let mut hits = Vec::new();
        for path in list.split('\0').filter(|p| p.ends_with(".md")) {
            let Some(owner) = Owner::of(path) else {
                continue;
            };
            let Some(text) = self.read_file_at("HEAD", path)? else {
                continue;
            };
            let lines: Vec<String> = text
                .lines()
                .filter(|l| l.to_lowercase().contains(&needle))
                .take(3)
                .map(|l| l.trim().to_string())
                .collect();
            if lines.is_empty() && !path.to_lowercase().contains(&needle) {
                continue;
            }
            let rank = match &owner {
                Owner::Person(p) if p == person => Rank::Own,
                Owner::Shared => Rank::Shared,
                Owner::Person(_) => Rank::Other,
            };
            let date = self
                .git
                .run(&["log", "-1", "--format=%aI", "HEAD", "--", path])?
                .trim()
                .to_string();
            hits.push(NoteHit {
                path: path.to_string(),
                owner,
                rank,
                date,
                lines,
            });
        }
        hits.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then_with(|| by_date_desc(&a.date, &b.date))
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(hits)
    }
}

fn by_date_desc(a: &str, b: &str) -> Ordering {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => y.cmp(&x),
        _ => b.cmp(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owners_from_paths() {
        assert_eq!(Owner::of("notes/runbook.md"), Some(Owner::Shared));
        assert_eq!(
            Owner::of("notes/ada/kiln.md"),
            Some(Owner::Person("ada".into()))
        );
        assert_eq!(
            Owner::of("notes/ada/deep/kiln.md"),
            Some(Owner::Person("ada".into()))
        );
        assert_eq!(Owner::of("facts/devices/ip.md"), None);
        assert_eq!(Owner::of("people/ada.md"), None);
        assert_eq!(Owner::of("notes/"), None);
    }

    #[test]
    fn paths_for_a_person() {
        assert_eq!(
            path_for("ada", "notes/kiln.md").unwrap(),
            "notes/ada/kiln.md"
        );
        assert_eq!(path_for("ada", "kiln.md").unwrap(), "notes/ada/kiln.md");
        assert_eq!(
            path_for("ada", "notes/ada/kiln.md").unwrap(),
            "notes/ada/kiln.md"
        );
        assert_eq!(
            path_for("ada", "notes/shop/kiln.md").unwrap(),
            "notes/ada/shop/kiln.md"
        );
        assert!(path_for("ada", "facts/devices/ip.md").is_err());
        assert!(path_for("ada", "people/ada.md").is_err());
        assert!(path_for("", "kiln.md").is_err());
        assert!(path_for("a/b", "kiln.md").is_err());
        assert!(path_for(".ada", "kiln.md").is_err());
        assert!(path_for("..", "kiln.md").is_err());
    }
}
