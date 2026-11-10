// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Numbered JSON records on main: `<dir>/<prefix><n>.json`.
//!
//! Proposals (`proposals/p3.json`), incidents (`incidents/i2.json`) and open
//! questions (`questions/ask1.json`) are all records like this. A record is
//! rewritten by one commit per change and never removed, so git keeps every
//! earlier version.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::memory::Memory;

/// `prefix` followed by one or more digits.
pub(crate) fn valid_id(prefix: &str, id: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

pub(crate) fn num(prefix: &str, id: &str) -> u64 {
    id.strip_prefix(prefix)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A record's text: pretty JSON and a final newline.
pub(crate) fn render<T: Serialize>(r: &T) -> String {
    let mut s = serde_json::to_string_pretty(r).expect("serializable");
    s.push('\n');
    s
}

impl Memory {
    /// Record ids in `dir` on `rev`, by number.
    pub(crate) fn record_ids(&self, rev: &str, dir: &str, prefix: &str) -> Result<Vec<String>> {
        let out = self.git.run(&[
            "ls-tree",
            "-r",
            "--name-only",
            rev,
            "--",
            &format!("{dir}/"),
        ])?;
        let mut ids: Vec<String> = out
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("{dir}/"))?.strip_suffix(".json"))
            .filter(|id| valid_id(prefix, id))
            .map(String::from)
            .collect();
        ids.sort_by_key(|id| num(prefix, id));
        Ok(ids)
    }

    /// The next free id in `dir` on `rev`.
    pub(crate) fn next_record_id(&self, rev: &str, dir: &str, prefix: &str) -> Result<String> {
        let n = self
            .record_ids(rev, dir, prefix)?
            .last()
            .map(|id| num(prefix, id))
            .unwrap_or(0);
        Ok(format!("{prefix}{}", n + 1))
    }

    /// One record; `unknown` builds the error for a bad or missing id.
    pub(crate) fn record_at<T: DeserializeOwned>(
        &self,
        rev: &str,
        dir: &str,
        prefix: &str,
        id: &str,
        unknown: impl Fn(String) -> Error,
    ) -> Result<T> {
        if !valid_id(prefix, id) {
            return Err(unknown(id.to_string()));
        }
        let path = format!("{dir}/{id}.json");
        let text = self
            .read_file_at(rev, &path)?
            .ok_or_else(|| unknown(id.to_string()))?;
        serde_json::from_str(&text).map_err(|e| Error::Malformed {
            path,
            reason: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(valid_id("i", "i1") && valid_id("ask", "ask12"));
        assert!(!valid_id("i", "i") && !valid_id("i", "p1") && !valid_id("ask", "ask1/../x"));
        assert_eq!(num("ask", "ask12"), 12);
    }
}
