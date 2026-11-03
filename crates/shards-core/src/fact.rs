// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! The fact file format: YAML front matter, then the value as the body.
//!
//! ```text
//! ---
//! address: "devices/rotoforge-ctl/ip"
//! updated: "2026-11-03T14:00:00Z"
//! author: "shard-7f3a"
//! human: "Raelyn"
//! why: "moved to the new switch"
//! ---
//! 10.0.4.40
//! ```
//!
//! Every front-matter value is written as a double-quoted string using JSON
//! escaping, which is also valid YAML. Keys are written in the fixed order
//! above; `human` is omitted when there is none. The body is the value
//! followed by exactly one newline.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontMatter {
    pub address: String,
    /// RFC 3339, UTC, written by the tool.
    pub updated: String,
    pub author: String,
    pub human: Option<String>,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub front: FrontMatter,
    pub value: String,
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization cannot fail")
}

impl Fact {
    pub fn render(&self) -> String {
        let f = &self.front;
        let mut out = String::from("---\n");
        out += &format!("address: {}\n", quote(&f.address));
        out += &format!("updated: {}\n", quote(&f.updated));
        out += &format!("author: {}\n", quote(&f.author));
        if let Some(h) = &f.human {
            out += &format!("human: {}\n", quote(h));
        }
        out += &format!("why: {}\n", quote(&f.why));
        out += "---\n";
        out += &self.value;
        out += "\n";
        out
    }

    /// Parse a fact file. `path` is only used in error messages.
    pub fn parse(text: &str, path: &str) -> Result<Self> {
        let bad = |reason: &str| Error::Malformed {
            path: path.to_string(),
            reason: reason.to_string(),
        };
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| bad("missing opening `---`"))?;
        let end = rest
            .find("\n---\n")
            .or_else(|| rest.strip_suffix("\n---").map(|r| r.len()))
            .ok_or_else(|| bad("missing closing `---`"))?;
        let header = &rest[..end];
        let body = rest[end..]
            .strip_prefix("\n---\n")
            .or_else(|| rest[end..].strip_prefix("\n---"))
            .unwrap_or("");
        let value = body.strip_suffix('\n').unwrap_or(body).to_string();

        let (mut address, mut updated, mut author, mut human, mut why) =
            (None, None, None, None, None);
        for line in header.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (key, raw) = line
                .split_once(':')
                .ok_or_else(|| bad(&format!("not a `key: value` line: {line:?}")))?;
            let raw = raw.trim();
            let val = if raw.starts_with('"') {
                serde_json::from_str::<String>(raw)
                    .map_err(|e| bad(&format!("bad quoted value for `{key}`: {e}")))?
            } else {
                raw.to_string()
            };
            match key.trim() {
                "address" => address = Some(val),
                "updated" => updated = Some(val),
                "author" => author = Some(val),
                "human" => human = Some(val).filter(|v| !v.is_empty()),
                "why" => why = Some(val),
                _ => {} // unknown keys are tolerated for forward compatibility
            }
        }
        Ok(Fact {
            front: FrontMatter {
                address: address.ok_or_else(|| bad("missing `address`"))?,
                updated: updated.ok_or_else(|| bad("missing `updated`"))?,
                author: author.ok_or_else(|| bad("missing `author`"))?,
                human,
                why: why.ok_or_else(|| bad("missing `why`"))?,
            },
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(human: Option<&str>, value: &str) -> Fact {
        Fact {
            front: FrontMatter {
                address: "devices/rotoforge-ctl/ip".into(),
                updated: "2026-11-03T14:00:00Z".into(),
                author: "shard-7f3a".into(),
                human: human.map(String::from),
                why: "moved: to the \"new\" switch".into(),
            },
            value: value.into(),
        }
    }

    #[test]
    fn round_trip() {
        for f in [
            sample(Some("Raelyn"), "10.0.4.40"),
            sample(None, "line one\n---\nline three"),
            sample(None, "trailing newline\n"),
        ] {
            let text = f.render();
            assert_eq!(Fact::parse(&text, "t").unwrap(), f, "{text}");
        }
    }

    #[test]
    fn rendered_shape() {
        let text = sample(Some("Raelyn"), "10.0.4.40").render();
        assert_eq!(
            text,
            "---\naddress: \"devices/rotoforge-ctl/ip\"\nupdated: \"2026-11-03T14:00:00Z\"\n\
             author: \"shard-7f3a\"\nhuman: \"Raelyn\"\nwhy: \"moved: to the \\\"new\\\" switch\"\n\
             ---\n10.0.4.40\n"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Fact::parse("no front matter", "t").is_err());
        assert!(Fact::parse("---\naddress: \"a/b\"\n---\nx\n", "t").is_err());
    }
}
