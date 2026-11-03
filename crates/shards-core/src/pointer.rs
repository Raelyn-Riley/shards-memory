// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Pointers: `{{fact:<address>}}` in notes and chat, resolved to the current value.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::address::Address;
use crate::error::{Error, Result};
use crate::memory::Memory;

#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOptions {
    /// Append the fact's address after the value: `10.0.4.40 [devices/x/ip]`.
    pub annotate: bool,
}

fn pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{\{fact:([^{}]*)\}\}").expect("valid regex"))
}

/// Every pointer address (raw, as written) in `text`, in order of appearance.
pub fn pointers(text: &str) -> Vec<String> {
    pattern()
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// Replace every `{{fact:<address>}}` with the current value.
///
/// If any pointer cannot be resolved (bad address or no such fact), nothing is
/// returned and the error names every unresolvable pointer.
pub fn resolve(mem: &Memory, text: &str, opts: ResolveOptions) -> Result<String> {
    let mut values: HashMap<String, String> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    for raw in pointers(text) {
        if values.contains_key(&raw) || missing.contains(&raw) {
            continue;
        }
        let found = Address::parse(&raw).and_then(|a| mem.get(&a));
        match found {
            Ok(f) => {
                let v = if opts.annotate {
                    format!("{} [{}]", f.value, f.address)
                } else {
                    f.value
                };
                values.insert(raw, v);
            }
            Err(Error::InvalidAddress { .. }) | Err(Error::UnknownAddress(_)) => missing.push(raw),
            Err(e) => return Err(e),
        }
    }
    if !missing.is_empty() {
        return Err(Error::Unresolvable { pointers: missing });
    }
    Ok(pattern()
        .replace_all(text, |c: &regex::Captures| values[&c[1]].clone())
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_pointers() {
        let t = "ip {{fact:devices/a/ip}} port {{fact:devices/a/port}} {{not:this}} {{fact:}}";
        assert_eq!(pointers(t), vec!["devices/a/ip", "devices/a/port", ""]);
    }
}
