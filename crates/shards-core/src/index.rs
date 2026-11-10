// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem index [section]`: every fact's address with a one-line description,
//! and **no values**, so the index can never become a second home for a
//! fact. Find the address here, then read the value with `mem get`.
//!
//! The address is the fact's path under `facts/` (her design, 11/6:
//! `devices/ctl/ip`). The description is the fact file's `description`
//! front-matter line: given with `mem set --description`, kept by every
//! `supersede`, editable by the core with `mem describe`. A description
//! that contains the value is refused when it is written.
//!
//! From main it lists main. From a shard's worktree it lists main as it is
//! now (so facts accepted since the shard opened show up), plus the shard's
//! own facts that aren't on main yet, marked `pending`: `new`, `changed` or
//! `removed` on this shard, waiting for its dinit and the core.

use serde::Serialize;

use crate::address::Address;
use crate::error::Result;
use crate::fact::Fact;
use crate::memory::{Context, Memory};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexEntry {
    pub address: String,
    pub description: Option<String>,
    /// On a shard: `new`, `changed` or `removed` on this shard, not on main
    /// yet. `None`: as on main.
    pub pending: Option<&'static str>,
}

/// Does `address` fall in `section` (`devices` or `devices/ctl`)?
fn in_section(address: &str, section: &str) -> bool {
    let s = section
        .trim()
        .trim_start_matches("facts/")
        .trim_end_matches(".md")
        .trim_matches('/');
    s.is_empty() || address == s || address.starts_with(&format!("{s}/"))
}

impl Memory {
    fn index_at(&self, rev: &str, section: &str) -> Result<Vec<(String, Option<String>)>> {
        let out = self
            .git
            .run(&["ls-tree", "-r", "--name-only", rev, "--", "facts/"])?;
        let mut v = Vec::new();
        for path in out.lines() {
            let Some(a) = Address::from_home(path) else {
                continue;
            };
            if !in_section(a.as_str(), section) {
                continue;
            }
            v.push((a.to_string(), self.description_at(rev, path)?));
        }
        Ok(v)
    }

    fn description_at(&self, rev: &str, path: &str) -> Result<Option<String>> {
        Ok(self
            .read_file_at(rev, path)?
            .and_then(|t| Fact::parse(&t, path).ok())
            .and_then(|f| f.front.description))
    }

    /// Every fact address in `section` (all of them for `None`), sorted,
    /// with its description and, on a shard, whether it is pending there.
    pub fn index(&self, section: Option<&str>) -> Result<Vec<IndexEntry>> {
        let section = section.unwrap_or("");
        let mut entries: std::collections::BTreeMap<String, IndexEntry> = self
            .index_at("main", section)?
            .into_iter()
            .map(|(address, description)| {
                (
                    address.clone(),
                    IndexEntry {
                        address,
                        description,
                        pending: None,
                    },
                )
            })
            .collect();
        if let Context::Shard(_) = self.context {
            let base = self.git.run(&["merge-base", "main", "HEAD"])?;
            let base = base.trim();
            let changed = self.git.run(&[
                "diff-tree",
                "-r",
                "--name-only",
                base,
                "HEAD",
                "--",
                "facts/",
            ])?;
            for path in changed.lines() {
                let Some(a) = Address::from_home(path) else {
                    continue;
                };
                if !in_section(a.as_str(), section) {
                    continue;
                }
                let on_main = entries.contains_key(a.as_str());
                let here = self.read_file_at("HEAD", path)?.is_some();
                let pending = match (here, on_main) {
                    (true, false) => "new",
                    (true, true) => "changed",
                    (false, _) => "removed",
                };
                let description = if here {
                    self.description_at("HEAD", path)?
                } else {
                    entries.get(a.as_str()).and_then(|e| e.description.clone())
                };
                entries.insert(
                    a.to_string(),
                    IndexEntry {
                        address: a.to_string(),
                        description,
                        pending: Some(pending),
                    },
                );
            }
        }
        Ok(entries.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::in_section;

    #[test]
    fn sections() {
        assert!(in_section("devices/ctl/ip", ""));
        assert!(in_section("devices/ctl/ip", "devices"));
        assert!(in_section("devices/ctl/ip", "devices/ctl/"));
        assert!(in_section("devices/ctl/ip", "facts/devices"));
        assert!(in_section("devices/ctl/ip", "devices/ctl/ip"));
        assert!(!in_section("devices/ctl/ip", "dev"));
        assert!(!in_section("devicesx/a", "devices"));
    }
}
