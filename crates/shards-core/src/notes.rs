// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Notes with owners.
//!
//! - `notes/<file>.md`, at the top of `notes/`, is a **shared** note.
//! - `notes/<person>/...` is that person's own note. The folder name is the
//!   person, in any case: `Dave`, `dave` and a folder `DAVE/` are the same
//!   person. A new folder is written lowercase; an existing one keeps its
//!   case.
//!
//! Notes are plain markdown, never facts: nothing here reads or writes
//! `facts/`, and a note's text is never taken as a fact's value. They are
//! written like any other `notes/` file, so in a shard they ride its branch
//! and merge at dinit like before.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::memory::{Context, Memory, Provenance};

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

/// Two person names are the same person when they match in any case.
pub fn same_person(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
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

/// The path of `path` in the folder `folder`. `path` may be given as
/// `notes/<file>.md`, `<file>.md`, or already as `notes/<person>/<file>.md`
/// with the person in any case; the result always uses `folder` as given.
pub fn path_for(folder: &str, path: &str) -> Result<String> {
    check_person(folder)?;
    let top = path.split('/').next().unwrap_or("");
    if ["facts", "people", "soul"].contains(&top) {
        return Err(Error::BadPath {
            path: path.to_string(),
            reason: "a person's note goes under notes/<person>/".to_string(),
        });
    }
    let rest = path.strip_prefix("notes/").unwrap_or(path);
    let rest = match rest.split_once('/') {
        Some((first, file)) if same_person(first, folder) => file,
        _ => rest,
    };
    Ok(format!("notes/{folder}/{rest}"))
}

impl Memory {
    /// The revisions notes are read from, newest edit first: HEAD, and in a
    /// shard main too (notes merged there after the shard opened).
    fn note_revs(&self) -> Vec<&'static str> {
        match self.context {
            Context::Shard(_) => vec!["HEAD", "refs/heads/main"],
            _ => vec!["HEAD"],
        }
    }

    /// The person folders under `notes/` at `rev`.
    fn person_folders(&self, rev: &str) -> Result<Vec<String>> {
        let list = self.git.run(&[
            "ls-tree",
            "-d",
            "-z",
            "--name-only",
            rev,
            "--",
            &format!("{DIR}/"),
        ])?;
        let mut v: Vec<String> = list
            .split('\0')
            .filter_map(|p| p.strip_prefix("notes/"))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        v.sort();
        Ok(v)
    }

    /// The folder `person`'s notes go in: an existing folder that matches in
    /// any case (on this branch, or in a shard on main), else the name in
    /// lowercase. If several folders match, one spelled exactly as given
    /// wins, then this branch's before main's, then the first by name.
    pub fn note_folder(&self, person: &str) -> Result<String> {
        check_person(person)?;
        let mut found = Vec::new();
        for rev in self.note_revs() {
            for f in self.person_folders(rev)? {
                if same_person(&f, person) && !found.contains(&f) {
                    found.push(f);
                }
            }
        }
        if found.iter().any(|f| f == person) {
            return Ok(person.to_string());
        }
        Ok(found
            .into_iter()
            .next()
            .unwrap_or_else(|| person.to_lowercase()))
    }

    /// Write a note: into `person`'s folder when given (see
    /// [`Memory::note_folder`]), else at `path` as before (a shared note at
    /// the top of `notes/`). Returns the path written and the commit.
    pub fn put_note_as(
        &self,
        person: Option<&str>,
        path: &str,
        text: &str,
        prov: &Provenance,
    ) -> Result<(String, String)> {
        let path = match person {
            Some(p) => path_for(&self.note_folder(p)?, path)?,
            None => path.to_string(),
        };
        let commit = self.put_file(&path, text, prov)?;
        Ok((path, commit))
    }

    /// Every note that contains `word` (case-insensitive, in its text or its
    /// path): `person`'s own first (their folder in any case), then the
    /// shared notes, then everyone else's; newest first within each.
    ///
    /// Notes are read from HEAD, and in a shard from main too, so notes
    /// merged after the shard opened show. A path on both counts once, with
    /// the shard's version. A note the shard deleted stays deleted.
    pub fn find_notes(&self, word: &str, person: &str) -> Result<Vec<NoteHit>> {
        check_person(person)?;
        let needle = word.trim().to_lowercase();
        if needle.is_empty() {
            return Err(Error::BadPath {
                path: word.to_string(),
                reason: "give a word to find".to_string(),
            });
        }
        let revs = self.note_revs();
        // In a shard: the notes at its base, to tell a note the shard deleted
        // from one main added later.
        let base: HashSet<String> = if revs.len() > 1 {
            let b = self.git.run(&["merge-base", revs[0], revs[1]])?;
            self.note_paths(b.trim())?.into_iter().collect()
        } else {
            HashSet::new()
        };
        let mut seen: HashSet<String> = HashSet::new();
        let mut hits = Vec::new();
        for (i, rev) in revs.iter().enumerate() {
            for path in self.note_paths(rev)? {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if i > 0 && base.contains(&path) {
                    continue;
                }
                if let Some(hit) = self.note_hit(rev, &path, &needle, person)? {
                    hits.push(hit);
                }
            }
        }
        hits.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then_with(|| by_date_desc(&a.date, &b.date))
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(hits)
    }

    /// Every `.md` path under `notes/` at `rev`.
    fn note_paths(&self, rev: &str) -> Result<Vec<String>> {
        let list = self.git.run(&[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            rev,
            "--",
            &format!("{DIR}/"),
        ])?;
        Ok(list
            .split('\0')
            .filter(|p| p.ends_with(".md"))
            .map(str::to_string)
            .collect())
    }

    fn note_hit(
        &self,
        rev: &str,
        path: &str,
        needle: &str,
        person: &str,
    ) -> Result<Option<NoteHit>> {
        let Some(owner) = Owner::of(path) else {
            return Ok(None);
        };
        let Some(text) = self.read_file_at(rev, path)? else {
            return Ok(None);
        };
        let lines: Vec<String> = text
            .lines()
            .filter(|l| l.to_lowercase().contains(needle))
            .take(3)
            .map(|l| l.trim().to_string())
            .collect();
        if lines.is_empty() && !path.to_lowercase().contains(needle) {
            return Ok(None);
        }
        let rank = match &owner {
            Owner::Person(p) if same_person(p, person) => Rank::Own,
            Owner::Shared => Rank::Shared,
            Owner::Person(_) => Rank::Other,
        };
        let date = self
            .git
            .run(&["log", "-1", "--format=%aI", rev, "--", path])?
            .trim()
            .to_string();
        Ok(Some(NoteHit {
            path: path.to_string(),
            owner,
            rank,
            date,
            lines,
        }))
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
        // The person in the path matches in any case; the folder given wins.
        assert_eq!(
            path_for("DAVE", "notes/dave/kiln.md").unwrap(),
            "notes/DAVE/kiln.md"
        );
        assert_eq!(
            path_for("dave", "notes/Dave/kiln.md").unwrap(),
            "notes/dave/kiln.md"
        );
        assert_eq!(
            path_for("dave", "notes/davey/kiln.md").unwrap(),
            "notes/dave/davey/kiln.md"
        );
    }

    #[test]
    fn people_match_in_any_case() {
        assert!(same_person("Dave", "dave"));
        assert!(same_person("DAVE", "dave"));
        assert!(same_person("Zoë", "ZOË"));
        assert!(!same_person("dave", "davey"));
    }
}
