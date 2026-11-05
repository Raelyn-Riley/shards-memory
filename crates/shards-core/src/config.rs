// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `shards.toml` at the memory root. Every setting has a default, so the file
//! (and every table and key in it) is optional. Unknown keys are an error, so
//! a typo can't silently fall back to a default.
//!
//! ```toml
//! [lock]
//! scope = "store"        # "store" | "section"
//! when_busy = "wait"     # "wait" | "fail"
//! timeout_secs = 30      # how long a waiting writer waits
//! lease_secs = 30        # lock lifetime without a heartbeat
//!
//! [soul]
//! admins = ["Ada"]       # humans who may approve soul changes
//!
//! [shards]
//! keep_branches = true   # keep shard/<id> after a clean dinit
//! worktree_dir = ".shards/worktrees"  # relative to the memory root, or absolute
//!
//! [activity]
//! source = "claude-code" # "claude-code" | "none"
//! transcript_dir = ""    # default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects
//!
//! [awareness]
//! idle_secs = 600        # no activity this long: idle
//! stale_secs = 3600      # no activity this long: stale
//! poll_secs = 30         # how often `mem watch` runs a pass
//!
//! [triggers]
//! same_fact = true
//! same_section = false
//! stale_with_work = true
//! open_conflict = true
//! soul_proposal = true
//!
//! [wake]
//! command = ""           # empty: record events, run nothing
//! timeout_secs = 300
//! cooldown_secs = 0      # at most one wake per this long; 0: no limit
//! ```

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

pub const FILE: &str = "shards.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// One lock for all of main.
    #[default]
    Store,
    /// One lock per top-level section.
    Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhenBusy {
    #[default]
    Wait,
    Fail,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LockConfig {
    pub scope: Scope,
    pub when_busy: WhenBusy,
    pub timeout_secs: f64,
    pub lease_secs: f64,
}

impl Default for LockConfig {
    fn default() -> Self {
        LockConfig {
            scope: Scope::Store,
            when_busy: WhenBusy::Wait,
            timeout_secs: 30.0,
            lease_secs: 30.0,
        }
    }
}

impl LockConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs_f64(self.timeout_secs.max(0.0))
    }
    pub fn lease(&self) -> Duration {
        Duration::from_secs_f64(self.lease_secs.max(0.1))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoulConfig {
    pub admins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShardsConfig {
    pub keep_branches: bool,
    pub worktree_dir: String,
}

impl Default for ShardsConfig {
    fn default() -> Self {
        ShardsConfig {
            keep_branches: true,
            worktree_dir: ".shards/worktrees".to_string(),
        }
    }
}

/// Where shard activity is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// Claude Code session transcripts (JSONL), found from the worktree path.
    #[default]
    ClaudeCode,
    /// Git only, no transcript.
    None,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::ClaudeCode => "claude-code",
            SourceKind::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityConfig {
    pub source: SourceKind,
    /// Overrides where transcripts are looked for. Empty: the agent tool's
    /// default. `~/` is expanded; a relative path is taken from the memory root.
    pub transcript_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AwarenessConfig {
    pub idle_secs: f64,
    pub stale_secs: f64,
    pub poll_secs: f64,
}

impl Default for AwarenessConfig {
    fn default() -> Self {
        AwarenessConfig {
            idle_secs: 600.0,
            stale_secs: 3600.0,
            poll_secs: 30.0,
        }
    }
}

impl AwarenessConfig {
    pub fn poll(&self) -> Duration {
        Duration::from_secs_f64(self.poll_secs.max(0.05))
    }
}

/// Which conditions fire an event. Each is on or off.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TriggersConfig {
    pub same_fact: bool,
    pub same_section: bool,
    pub stale_with_work: bool,
    pub open_conflict: bool,
    pub soul_proposal: bool,
}

impl Default for TriggersConfig {
    fn default() -> Self {
        TriggersConfig {
            same_fact: true,
            same_section: false,
            stale_with_work: true,
            open_conflict: true,
            soul_proposal: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakeConfig {
    /// Run with `sh -c`; pending events as JSON on stdin. Empty: run nothing.
    pub command: String,
    pub timeout_secs: f64,
    pub cooldown_secs: f64,
}

impl Default for WakeConfig {
    fn default() -> Self {
        WakeConfig {
            command: String::new(),
            timeout_secs: 300.0,
            cooldown_secs: 0.0,
        }
    }
}

impl WakeConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs_f64(self.timeout_secs.max(0.1))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub lock: LockConfig,
    pub soul: SoulConfig,
    pub shards: ShardsConfig,
    pub activity: ActivityConfig,
    pub awareness: AwarenessConfig,
    pub triggers: TriggersConfig,
    pub wake: WakeConfig,
}

impl Config {
    /// Load `shards.toml` from the memory root; a missing file means defaults.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config(e.to_string()))
    }
}

/// The file `mem init` writes.
pub const DEFAULT_TOML: &str = r#"# shards configuration. Edit freely; no code changes needed.

[lock]
scope = "store"        # "store" (one lock for main) | "section" (one per top-level section)
when_busy = "wait"     # "wait" (queue up to timeout_secs) | "fail" (fail fast)
timeout_secs = 30
lease_secs = 30        # a lock not renewed for this long, whose process is gone, is recovered

[soul]
admins = []            # humans allowed to approve soul changes, e.g. ["Ada"]

[shards]
keep_branches = true   # keep shard/<id> after a clean dinit (history); false deletes it
worktree_dir = ".shards/worktrees"

[activity]
source = "claude-code" # "claude-code" (read session transcripts) | "none" (git only)
transcript_dir = ""    # empty: ~/.claude/projects (or $CLAUDE_CONFIG_DIR/projects)

[awareness]
idle_secs = 600        # no activity for this long: idle
stale_secs = 3600      # no activity for this long: stale
poll_secs = 30         # how often `mem watch` looks

[triggers]
same_fact = true        # two open shards have the same fact in flight
same_section = false    # two open shards are in flight in the same section
stale_with_work = true  # stale shard with facts in flight
open_conflict = true    # a conflict record waits on main
soul_proposal = true    # a soul proposal waits for an admin

[wake]
command = ""           # run when a trigger fires (sh -c, events as JSON on stdin); empty: record only
timeout_secs = 300
cooldown_secs = 0      # wake the core at most once per this long; 0: no rate limit
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_parsing() {
        let d = Config::parse("").unwrap();
        assert_eq!(d.lock.scope, Scope::Store);
        assert_eq!(d.lock.when_busy, WhenBusy::Wait);
        assert!(d.shards.keep_branches);
        let c = Config::parse(DEFAULT_TOML).unwrap();
        assert_eq!(c.lock.timeout_secs, 30.0);
        let c = Config::parse(
            "[lock]\nscope = \"section\"\nwhen_busy = \"fail\"\n[soul]\nadmins=[\"Ada\"]\n",
        )
        .unwrap();
        assert_eq!(c.lock.scope, Scope::Section);
        assert_eq!(c.lock.when_busy, WhenBusy::Fail);
        assert_eq!(c.soul.admins, vec!["Ada"]);
        assert!(Config::parse("[lock]\nscoop = \"store\"\n").is_err());
        assert!(Config::parse("[lock]\nscope = \"everything\"\n").is_err());
    }

    #[test]
    fn awareness_sections() {
        let d = Config::parse(DEFAULT_TOML).unwrap();
        assert_eq!(d.activity.source, SourceKind::ClaudeCode);
        assert!(d.triggers.same_fact && !d.triggers.same_section);
        assert_eq!(d.wake.command, "");
        assert_eq!(d.wake.cooldown_secs, 0.0);
        assert_eq!(Config::parse("").unwrap().wake.cooldown_secs, 0.0);
        let c = Config::parse(
            "[activity]\nsource = \"none\"\n[triggers]\nsame_section = true\n[wake]\ncooldown_secs = 5\n",
        )
        .unwrap();
        assert_eq!(c.activity.source, SourceKind::None);
        assert!(c.triggers.same_section && c.triggers.same_fact);
        assert_eq!(c.wake.cooldown_secs, 5.0);
        assert!(Config::parse("[triggers]\nsame_facts = true\n").is_err());
        assert!(Config::parse("[wake]\ncmd = \"x\"\n").is_err());
        assert!(Config::parse("[activity]\nsource = \"vim\"\n").is_err());
        assert!(Config::parse("[awareness]\nidle = 5\n").is_err());
    }
}
