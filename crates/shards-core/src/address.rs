// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Fact addresses: `section/.../name`, mapped 1:1 to `facts/section/.../name.md`.

use std::fmt;

use crate::error::{Error, Result};

/// A validated fact address.
///
/// Rules: at least two `/`-separated segments (a section and a name); each
/// segment is lowercase ASCII letters, digits, `-`, `_` or `.`, starts with a
/// letter or digit, and does not end in `.md`. Lowercase-only keeps one fact
/// from getting a second home that differs only by case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(String);

impl Address {
    pub fn parse(raw: &str) -> Result<Self> {
        let bad = |reason: &str| Error::InvalidAddress {
            address: raw.to_string(),
            reason: reason.to_string(),
        };
        let s = raw.trim();
        if s.is_empty() {
            return Err(bad("empty"));
        }
        if s.starts_with("facts/") {
            return Err(bad("leave off the `facts/` prefix"));
        }
        if s.ends_with(".md") {
            return Err(bad("leave off the `.md` suffix"));
        }
        let segments: Vec<&str> = s.split('/').collect();
        if segments.len() < 2 {
            return Err(bad("needs a section and a name, e.g. `devices/printer/ip`"));
        }
        for seg in &segments {
            if seg.is_empty() {
                return Err(bad("empty path segment"));
            }
            let first = seg.as_bytes()[0];
            if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
                return Err(bad(
                    "each segment must start with a lowercase letter or digit",
                ));
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
            {
                return Err(bad("segments may only use a-z, 0-9, `-`, `_`, `.`"));
            }
            if seg.ends_with(".md") {
                return Err(bad("segments must not end in `.md`"));
            }
        }
        Ok(Address(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The top-level section, e.g. `devices` for `devices/rotoforge-ctl/ip`.
    pub fn section(&self) -> &str {
        self.0.split('/').next().expect("validated")
    }

    /// The last segment, e.g. `ip` for `devices/rotoforge-ctl/ip`.
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().expect("validated")
    }

    /// Repo-relative home path, e.g. `facts/devices/rotoforge-ctl/ip.md`.
    pub fn home(&self) -> String {
        format!("facts/{}.md", self.0)
    }

    /// Inverse of [`Address::home`].
    pub fn from_home(path: &str) -> Option<Self> {
        let inner = path.strip_prefix("facts/")?.strip_suffix(".md")?;
        Address::parse(inner).ok()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_addresses() {
        let a = Address::parse("devices/rotoforge-ctl/ip").unwrap();
        assert_eq!(a.section(), "devices");
        assert_eq!(a.name(), "ip");
        assert_eq!(a.home(), "facts/devices/rotoforge-ctl/ip.md");
        assert_eq!(Address::from_home(&a.home()), Some(a));
        assert!(Address::parse("constants/g_0").is_ok());
        assert!(Address::parse("net/v1.2").is_ok());
    }

    #[test]
    fn invalid_addresses() {
        for bad in [
            "",
            "ip",
            "facts/devices/ip",
            "devices/ip.md",
            "devices//ip",
            "/devices/ip",
            "devices/ip/",
            "devices/../ip",
            "devices/.hidden",
            "Devices/ip",
            "devices/my ip",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
