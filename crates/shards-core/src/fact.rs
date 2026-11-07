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
//! above; `human` is omitted when there is none. A fact accepted from a
//! shard's proposal adds `proposal` (`p3#2`), `reviewed_by` and
//! `review_why` after `why`: `author`, `human` and `why` stay the shard's.
//! The body is the value followed by exactly one newline.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub address: String,
    /// RFC 3339, UTC, written by the tool.
    pub updated: String,
    pub author: String,
    pub human: Option<String>,
    pub why: String,
    /// The proposal change this value was accepted from, e.g. `p3#2`.
    pub proposal: Option<String>,
    /// Who accepted it (the core, or a human).
    pub reviewed_by: Option<String>,
    /// The reviewer's reason.
    pub review_why: Option<String>,
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
        let mut pairs = vec![
            ("address", f.address.as_str()),
            ("updated", f.updated.as_str()),
            ("author", f.author.as_str()),
        ];
        if let Some(h) = &f.human {
            pairs.push(("human", h.as_str()));
        }
        pairs.push(("why", f.why.as_str()));
        for (k, v) in [
            ("proposal", &f.proposal),
            ("reviewed_by", &f.reviewed_by),
            ("review_why", &f.review_why),
        ] {
            if let Some(v) = v {
                pairs.push((k, v.as_str()));
            }
        }
        render_front(&pairs, &format!("{}\n", self.value))
    }

    /// Parse a fact file. `path` is only used in error messages.
    pub fn parse(text: &str, path: &str) -> Result<Self> {
        let bad = |reason: &str| Error::Malformed {
            path: path.to_string(),
            reason: reason.to_string(),
        };
        let (pairs, body) = parse_front(text, path)?;
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let value = body.strip_suffix('\n').unwrap_or(&body).to_string();
        Ok(Fact {
            front: FrontMatter {
                address: get("address").ok_or_else(|| bad("missing `address`"))?,
                updated: get("updated").ok_or_else(|| bad("missing `updated`"))?,
                author: get("author").ok_or_else(|| bad("missing `author`"))?,
                human: get("human").filter(|v| !v.is_empty()),
                why: get("why").ok_or_else(|| bad("missing `why`"))?,
                proposal: get("proposal").filter(|v| !v.is_empty()),
                reviewed_by: get("reviewed_by").filter(|v| !v.is_empty()),
                review_why: get("review_why").filter(|v| !v.is_empty()),
            },
            value,
        })
    }
}

/// Render `---` front matter (every value JSON-quoted) followed by `body`.
pub fn render_front(pairs: &[(&str, &str)], body: &str) -> String {
    let mut out = String::from("---\n");
    for (k, v) in pairs {
        out += &format!("{k}: {}\n", quote(v));
    }
    out += "---\n";
    out += body;
    out
}

/// Split a file into its front-matter `key: value` pairs and the body.
/// Values may be JSON-quoted or bare. Unknown keys are kept.
pub fn parse_front(text: &str, path: &str) -> Result<(Vec<(String, String)>, String)> {
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
    let mut pairs = Vec::new();
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
        pairs.push((key.trim().to_string(), val));
    }
    Ok((pairs, body.to_string()))
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
                ..Default::default()
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
            Fact {
                front: FrontMatter {
                    proposal: Some("p3#2".into()),
                    reviewed_by: Some("core".into()),
                    review_why: Some("matches the label".into()),
                    ..sample(Some("Ada"), "x").front
                },
                value: "10.0.4.41".into(),
            },
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
