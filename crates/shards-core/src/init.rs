// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! The inits: who the agent is, given to every session as its first context.
//!
//! They live in the memory's soul, so they follow the soul's rules (changing
//! one on main needs a human admin; from a shard it is a soul proposal):
//!
//! - `soul/init-core.md`: the core's init.
//! - `soul/init-shard.md`: every shard's init.
//! - `soul/name.md`: the agent's name, picked once by the core on its first
//!   boot (`mem core name`). After that only an admin changes it.
//!
//! `mem init` writes the two inits from the templates in this crate
//! (`templates/`), with site-specific words left as placeholders (`<org>`,
//! `<answers-to>`) for an admin to fill in. `<name>` is filled in from
//! `soul/name.md` when the init is delivered.
//!
//! Delivery always reads main's committed soul (never a worktree's copy or
//! an uncommitted edit), so a shard can't change its own init. With Claude
//! Code the init rides as an appended system prompt on every launch (fresh,
//! resume, watchdog restore, handoff); see `coreadapter.rs` for why. A
//! shard's init is followed by its session context ([`Memory::shard_context`]).

use crate::error::{Error, Result};
use crate::memory::{Context, Memory, Provenance};

pub const CORE_INIT: &str = "soul/init-core.md";
pub const SHARD_INIT: &str = "soul/init-shard.md";
pub const NAME: &str = "soul/name.md";

/// The core's init template (her draft; site-specific words as placeholders).
pub const TEMPLATE_CORE: &str = include_str!("../templates/init-core.md");
/// The shard's init template.
pub const TEMPLATE_SHARD: &str = include_str!("../templates/init-shard.md");

/// How many open quirks a shard's context lists, newest first.
const CONTEXT_QUIRKS: usize = 5;

fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    let bad = |why: &str| Error::BadName(format!("`{n}`: {why}"));
    if n.is_empty() {
        return Err(bad("a name can't be empty"));
    }
    if n.chars().count() > 40 {
        return Err(bad("keep it under 40 characters"));
    }
    if n.contains(['\n', '\r', '<', '>', '`']) {
        return Err(bad("one line, no <, > or backticks"));
    }
    Ok(n.to_string())
}

impl Memory {
    /// The agent's name, from main's `soul/name.md`; `None` until the core
    /// has picked it.
    pub fn agent_name(&self) -> Result<Option<String>> {
        Ok(self
            .main()?
            .read_file_at("main", NAME)?
            .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
            .filter(|n| !n.is_empty()))
    }

    /// The core picks its name, once, on main. A second pick is refused:
    /// after the first, the name is soul, and only an admin changes it
    /// (`mem put soul/name.md --human <admin>`). Not from a shard.
    pub fn core_name(&self, name: &str, prov: &Provenance) -> Result<String> {
        if let Context::Shard(s) = &self.context {
            return Err(Error::BadName(format!(
                "shard `{s}` can't pick the name; the core picks it once"
            )));
        }
        if std::env::var("SHARDS_SHARD").is_ok_and(|s| !s.trim().is_empty()) {
            return Err(Error::BadName(
                "a shard's pane can't pick the name; the core picks it once".into(),
            ));
        }
        let name = check_name(name)?;
        let main = self.main()?;
        let mut prov = prov.clone();
        if prov.author.trim().is_empty() {
            prov.author = "core".into();
        }
        let prov = main.effective(&prov, true)?;
        let path = NAME.to_string();
        main.locked_commit(
            "name",
            &name,
            std::slice::from_ref(&path),
            &prov,
            &[("Name", name.clone())],
            |m, _| {
                if let Some(old) = m.read_file_at("HEAD", NAME)? {
                    return Err(Error::BadName(format!(
                        "the name is already picked ({}); it's picked once. Only a human admin \
                         changes it: mem put {NAME} --human <admin> --why \"...\"",
                        old.trim()
                    )));
                }
                Ok(vec![(path.clone(), Some(format!("{name}\n")))])
            },
        )
    }

    /// An init as main has it, with `<name>` filled in. `None` if this
    /// memory has no such init (made before inits existed).
    fn init_text(&self, path: &str) -> Result<Option<(String, Option<String>)>> {
        let main = self.main()?;
        let Some(text) = main.read_file_at("main", path)? else {
            return Ok(None);
        };
        let name = main.agent_name()?;
        let text = match &name {
            Some(n) => text.replace("<name>", n),
            None => text,
        };
        Ok(Some((text, name)))
    }

    /// The core's first context: its init, and, until it has one, the
    /// instruction to pick its name.
    pub fn core_init(&self) -> Result<Option<String>> {
        let Some((mut text, name)) = self.init_text(CORE_INIT)? else {
            return Ok(None);
        };
        if name.is_none() {
            text = format!(
                "{}\n\n---\n\n## Your name\n\nYou haven't picked your name yet: `<name>` above \
                 stands for it. Pick it now, once. It's yours for good, and every shard goes by \
                 it. Record it with `mem core name <name> --why \"...\"`. After that, only a \
                 human admin can change it.\n",
                text.trim_end()
            );
        }
        Ok(Some(text))
    }

    /// A shard's first context: its init, then `mem shard context`.
    pub fn shard_init(&self, id: &str) -> Result<Option<String>> {
        let Some((mut text, name)) = self.init_text(SHARD_INIT)? else {
            return Ok(None);
        };
        text = text.trim_end().to_string();
        if name.is_none() {
            text.push_str(
                "\n\n---\n\nThe core hasn't picked a name yet: `<name>` above stands for it. \
                 Don't make one up; if someone asks, say the core hasn't picked it yet.",
            );
        }
        Ok(Some(format!(
            "{text}\n\n---\n\n# This session\n\nWhat `mem shard context` said when this \
             session started (run it again any time):\n\n{}",
            self.shard_context(Some(id))?
        )))
    }

    /// `mem shard context`: what a shard should know at the start of a
    /// session: who it is with, their profile, the open questions this
    /// person may answer, and the open quirks.
    pub fn shard_context(&self, id: Option<&str>) -> Result<String> {
        let main = self.main()?;
        let human = match id {
            Some(id) => main.shard_meta(id).ok().and_then(|m| m.human),
            None => None,
        };
        let mut s = match id {
            Some(id) => format!(
                "You are shard {id}, with {}.\n",
                human.as_deref().unwrap_or("no one named yet")
            ),
            None => "Not in a shard.\n".to_string(),
        };
        if let Some(h) = &human {
            match main.profile_path(h)? {
                Some(p) => {
                    s += &format!("Their profile: {p} (read main's copy: git show main:{p}).\n")
                }
                None => {
                    s += &format!(
                        "No profile for {h} yet; if you learn how they like to work, write \
                     people/{}.md with mem put.\n",
                        h.to_lowercase()
                    )
                }
            }
        }
        s.push('\n');
        s += &main.questions_brief(human.as_deref())?;
        let mut open: Vec<_> = main
            .quirks()?
            .into_iter()
            .filter(|q| q.state == crate::quirk::QuirkState::Open)
            .collect();
        open.reverse();
        s.push('\n');
        if open.is_empty() {
            s += "No open quirks.\n";
        } else {
            s += &format!(
                "Open quirks ({}{}). Mention one when someone is about to use that tool:\n",
                open.len(),
                if open.len() > CONTEXT_QUIRKS {
                    format!(", newest {CONTEXT_QUIRKS}; mem quirk list for all")
                } else {
                    String::new()
                }
            );
            for q in open.iter().take(CONTEXT_QUIRKS) {
                s += &format!(
                    "  {} {}: {} (reported by {}, {}; status: {})\n",
                    q.id,
                    q.tool,
                    q.quirk,
                    q.reported_by,
                    q.reported.get(..10).unwrap_or(&q.reported),
                    q.status
                );
            }
        }
        Ok(s)
    }

    /// `people/<person>.md` on main, matched in any case.
    fn profile_path(&self, person: &str) -> Result<Option<String>> {
        let out = self
            .git
            .run(&["ls-tree", "--name-only", "main", "--", "people/"])?;
        let want = format!("people/{}.md", person.to_lowercase());
        Ok(out
            .lines()
            .find(|p| p.to_lowercase() == want)
            .map(String::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_have_placeholders_and_no_site_names() {
        for t in [TEMPLATE_CORE, TEMPLATE_SHARD] {
            assert!(t.contains("<name>") && t.contains("<org>") && t.contains("<answers-to>"));
            assert!(!t.contains("Sindry"));
            assert!(t.contains("mem index"));
            assert!(!t.contains("grep -ri"));
            assert!(!t.contains("(to be built)*"));
        }
        assert!(TEMPLATE_CORE.contains("harmless check"));
    }

    #[test]
    fn names() {
        assert!(check_name("Wren").is_ok());
        assert!(check_name(" ").is_err());
        assert!(check_name("a\nb").is_err());
        assert!(check_name("<name>").is_err());
    }
}
