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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub lock: LockConfig,
    pub soul: SoulConfig,
    pub shards: ShardsConfig,
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
}
