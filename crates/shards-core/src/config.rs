// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `shards.toml` at the memory root. Every setting has a default, so the file
//! (and every table and key in it) is optional. Unknown keys are an error, so
//! a typo can't silently fall back to a default.
//!
//! Every duration lives in one `[intervals]` section. The same settings are
//! still read from their old sections (`[lock] timeout_secs`, `[core]
//! keep_warm_mins`, `[quirks] archive_after_days`, ...), so an older
//! `shards.toml` keeps working. When a setting is in both places,
//! `[intervals]` wins.
//!
//! ```toml
//! [intervals]
//! lock_timeout_secs = 30        # how long a waiting writer waits for a lock
//! lock_lease_secs = 30          # lock lifetime without a heartbeat
//! idle_secs = 600               # no shard activity this long: idle
//! stale_secs = 3600             # no shard activity this long: stale
//! watch_poll_secs = 30          # how often `mem watch` runs a pass
//! wake_timeout_secs = 300       # how long a wake command may run
//! core_cooldown_secs = 0        # wake the core at most once per this long; 0: no limit
//! core_start_timeout_secs = 60  # a core/shard session coming up and pinning
//! core_exit_timeout_secs = 30   # a core/shard session exiting when told to
//! core_land_timeout_secs = 10   # a typed line showing in the pane / transcript
//! keep_warm_mins = 50           # keep-warm ping after this long idle
//! quirk_archive_days = 90       # `mem quirk archive`: fixed and not mentioned this long
//!
//! [lock]
//! scope = "store"        # "store" | "section"
//! when_busy = "wait"     # "wait" | "fail"
//!
//! [soul]
//! admins = ["Ada"]       # humans who may approve soul changes
//!
//! [shards]
//! keep_branches = true   # keep shard/<id> after a clean dinit
//! worktree_dir = ".shards/worktrees"  # relative to the memory root, or absolute
//! command = "claude"     # each shard's session, run in its own tmux pane
//! adapter = "claude-code" # "claude-code" | "plain"
//! socket = ".shards/sessions/tmux.sock"
//! claude_config = ""     # default: $CLAUDE_CONFIG_DIR/.claude.json or ~/.claude.json
//!
//! [activity]
//! source = "claude-code" # "claude-code" | "none"
//! transcript_dir = ""    # default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects
//!
//! [triggers]
//! same_fact = true
//! same_section = false
//! stale_with_work = true
//! open_conflict = true
//! soul_proposal = true
//!
//! [wake]
//! mode = "auto"          # "auto" | "command" | "pane"
//! command = ""           # empty: record events, run nothing
//!
//! [core]
//! command = "claude"     # the reasoning core, run in a tmux pane
//! adapter = "claude-code" # "claude-code" | "plain"
//! socket = ".shards/core/tmux.sock"
//! session = "core"
//! cwd = ""               # empty: the memory root
//! watchdog = true        # mem watch restores a dead core
//! keep_warm = false
//! keep_warm_quiet_limit = 8  # a count, not a duration: stays in [core]
//! ```
//!
//! Old places, still read (`[intervals]` wins):
//!
//! | `[intervals]` | old place |
//! |---|---|
//! | `lock_timeout_secs` | `[lock] timeout_secs` |
//! | `lock_lease_secs` | `[lock] lease_secs` |
//! | `idle_secs` | `[awareness] idle_secs` |
//! | `stale_secs` | `[awareness] stale_secs` |
//! | `watch_poll_secs` | `[awareness] poll_secs` |
//! | `wake_timeout_secs` | `[wake] timeout_secs` |
//! | `core_cooldown_secs` | `[wake] cooldown_secs` |
//! | `core_start_timeout_secs` | `[core] start_timeout_secs` |
//! | `core_exit_timeout_secs` | `[core] exit_timeout_secs` |
//! | `core_land_timeout_secs` | `[core] land_timeout_secs` |
//! | `keep_warm_mins` | `[core] keep_warm_mins` |
//! | `quirk_archive_days` | `[quirks] archive_after_days` |
//!
//! After loading, the resolved values sit in the old section structs too
//! (`cfg.lock.timeout_secs`, ...), so the code reading them is unchanged.

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
    /// The command line `mem shard start` types into a shard's pane.
    pub command: String,
    pub adapter: AdapterKind,
    /// The shards' one tmux server: relative to the memory root, or absolute.
    pub socket: String,
    /// Claude Code's config file, read (never written) to check that the
    /// memory root is trusted. Empty: `$CLAUDE_CONFIG_DIR/.claude.json` or
    /// `~/.claude.json`.
    pub claude_config: String,
}

impl Default for ShardsConfig {
    fn default() -> Self {
        ShardsConfig {
            keep_branches: true,
            worktree_dir: ".shards/worktrees".to_string(),
            command: "claude".to_string(),
            adapter: AdapterKind::ClaudeCode,
            socket: ".shards/sessions/tmux.sock".to_string(),
            claude_config: String::new(),
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

/// How `mem watch` wakes the reasoning core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WakeMode {
    /// `pane` once a core has been started with `mem core start` (and not
    /// stopped), otherwise `command`.
    #[default]
    Auto,
    /// Run `[wake] command` with the events on stdin.
    Command,
    /// Type one short line into the core's tmux pane.
    Pane,
}

impl WakeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WakeMode::Auto => "auto",
            WakeMode::Command => "command",
            WakeMode::Pane => "pane",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakeConfig {
    pub mode: WakeMode,
    /// Run with `sh -c`; pending events as JSON on stdin. Empty: run nothing.
    pub command: String,
    pub timeout_secs: f64,
    pub cooldown_secs: f64,
}

impl Default for WakeConfig {
    fn default() -> Self {
        WakeConfig {
            mode: WakeMode::Auto,
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

/// How the core's session is driven: where its transcripts are, how it is
/// resumed and how it is told to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    /// Claude Code: session ids from its transcripts, `--resume <id>`, `/exit`.
    #[default]
    ClaudeCode,
    /// Any line-reading program: no session id, no resume, exit by `/exit`.
    Plain,
}

impl AdapterKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdapterKind::ClaudeCode => "claude-code",
            AdapterKind::Plain => "plain",
        }
    }
}

/// The reasoning core as a long-lived session in a tmux pane.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// The command line that starts the core (typed into the pane's shell).
    pub command: String,
    pub adapter: AdapterKind,
    /// The tmux socket: relative to the memory root, or absolute.
    pub socket: String,
    /// The tmux session name.
    pub session: String,
    /// The core's working directory. Empty: the memory root.
    pub cwd: String,
    /// `mem watch` runs `mem core check` every pass once a core is started.
    pub watchdog: bool,
    /// How long a start, resume or handoff may take to come up and pin.
    pub start_timeout_secs: f64,
    /// How long the core may take to exit when told to.
    pub exit_timeout_secs: f64,
    /// How long to wait for a typed line to show in the pane / transcript.
    pub land_timeout_secs: f64,
    pub keep_warm: bool,
    pub keep_warm_mins: f64,
    pub keep_warm_quiet_limit: u32,
}

impl Default for CoreConfig {
    fn default() -> Self {
        CoreConfig {
            command: "claude".to_string(),
            adapter: AdapterKind::ClaudeCode,
            socket: ".shards/core/tmux.sock".to_string(),
            session: "core".to_string(),
            cwd: String::new(),
            watchdog: true,
            start_timeout_secs: 60.0,
            exit_timeout_secs: 30.0,
            land_timeout_secs: 10.0,
            keep_warm: false,
            keep_warm_mins: 50.0,
            keep_warm_quiet_limit: 8,
        }
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.05))
}

impl CoreConfig {
    pub fn start_timeout(&self) -> Duration {
        secs(self.start_timeout_secs)
    }
    pub fn exit_timeout(&self) -> Duration {
        secs(self.exit_timeout_secs)
    }
    pub fn land_timeout(&self) -> Duration {
        secs(self.land_timeout_secs)
    }
}

/// The known-quirks board.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuirksConfig {
    /// `mem quirk archive` archives fixed quirks not mentioned for this long.
    pub archive_after_days: f64,
}

impl Default for QuirksConfig {
    fn default() -> Self {
        QuirksConfig {
            archive_after_days: 90.0,
        }
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
    pub core: CoreConfig,
    pub quirks: QuirksConfig,
    /// Every duration in one place; see the module docs.
    pub intervals: IntervalsConfig,
}

/// `[intervals]`: every duration. A key left out falls back to its old place
/// (or that place's default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntervalsConfig {
    pub lock_timeout_secs: Option<f64>,
    pub lock_lease_secs: Option<f64>,
    pub idle_secs: Option<f64>,
    pub stale_secs: Option<f64>,
    pub watch_poll_secs: Option<f64>,
    pub wake_timeout_secs: Option<f64>,
    pub core_cooldown_secs: Option<f64>,
    pub core_start_timeout_secs: Option<f64>,
    pub core_exit_timeout_secs: Option<f64>,
    pub core_land_timeout_secs: Option<f64>,
    pub keep_warm_mins: Option<f64>,
    pub quirk_archive_days: Option<f64>,
}

impl Config {
    /// Load `shards.toml` from the memory root; a missing file means defaults.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::parse(""),
            Err(e) => Err(e.into()),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        let mut c: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        c.resolve_intervals();
        Ok(c)
    }

    /// `[intervals]` wins over the old places; afterwards both agree.
    fn resolve_intervals(&mut self) {
        fn pick(new: &mut Option<f64>, old: &mut f64) {
            match *new {
                Some(v) => *old = v,
                None => *new = Some(*old),
            }
        }
        let i = &mut self.intervals;
        pick(&mut i.lock_timeout_secs, &mut self.lock.timeout_secs);
        pick(&mut i.lock_lease_secs, &mut self.lock.lease_secs);
        pick(&mut i.idle_secs, &mut self.awareness.idle_secs);
        pick(&mut i.stale_secs, &mut self.awareness.stale_secs);
        pick(&mut i.watch_poll_secs, &mut self.awareness.poll_secs);
        pick(&mut i.wake_timeout_secs, &mut self.wake.timeout_secs);
        pick(&mut i.core_cooldown_secs, &mut self.wake.cooldown_secs);
        pick(
            &mut i.core_start_timeout_secs,
            &mut self.core.start_timeout_secs,
        );
        pick(
            &mut i.core_exit_timeout_secs,
            &mut self.core.exit_timeout_secs,
        );
        pick(
            &mut i.core_land_timeout_secs,
            &mut self.core.land_timeout_secs,
        );
        pick(&mut i.keep_warm_mins, &mut self.core.keep_warm_mins);
        pick(
            &mut i.quirk_archive_days,
            &mut self.quirks.archive_after_days,
        );
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
command = "claude"     # typed into each shard's pane by `mem shard start`
adapter = "claude-code" # "claude-code": pinned --session-id, --resume <id> | "plain"
socket = ".shards/sessions/tmux.sock"  # the shards' one tmux server
claude_config = ""     # empty: ~/.claude.json (read to check the memory root is trusted)

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
mode = "auto"          # "auto": "pane" once `mem core start` has run, else "command" | "command" | "pane"
command = ""           # command mode: run this (sh -c, events as JSON on stdin); empty: record only
timeout_secs = 300
cooldown_secs = 0      # wake the core at most once per this long; 0: no rate limit

[core]                 # the reasoning core: one long-lived session in a tmux pane
command = "claude"     # typed into the pane's shell by `mem core start`
adapter = "claude-code" # "claude-code" (session ids, --resume <id>) | "plain"
socket = ".shards/core/tmux.sock"  # its own tmux server
session = "core"
cwd = ""               # empty: the memory root
watchdog = true        # mem watch restores a dead core and resumes its pinned session
start_timeout_secs = 60
exit_timeout_secs = 30
land_timeout_secs = 10
keep_warm = false      # ping an idle core to keep its prompt cache warm (for measuring)
keep_warm_mins = 50
keep_warm_quiet_limit = 8  # stop after this many pings with no real activity
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

    #[test]
    fn core_section() {
        let d = Config::parse(DEFAULT_TOML).unwrap();
        assert_eq!(d.core.command, "claude");
        assert_eq!(d.core.socket, ".shards/core/tmux.sock");
        assert_eq!(d.core.session, "core");
        assert!(d.core.watchdog && !d.core.keep_warm);
        assert_eq!(d.core.keep_warm_mins, 50.0);
        assert_eq!(d.core.keep_warm_quiet_limit, 8);
        assert_eq!(d.wake.mode, WakeMode::Auto);
        let e = Config::parse("").unwrap();
        assert_eq!(e.core.command, "claude");
        assert_eq!(e.wake.mode, WakeMode::Auto);
        let c = Config::parse("[wake]\nmode = \"pane\"\n[core]\nadapter = \"plain\"\n").unwrap();
        assert_eq!(c.wake.mode, WakeMode::Pane);
        assert_eq!(c.core.adapter, AdapterKind::Plain);
        assert!(Config::parse("[wake]\nmode = \"tty\"\n").is_err());
        assert!(Config::parse("[core]\nkeepwarm = true\n").is_err());
    }

    #[test]
    fn quirks_section() {
        assert_eq!(Config::parse("").unwrap().quirks.archive_after_days, 90.0);
        assert_eq!(
            Config::parse(DEFAULT_TOML)
                .unwrap()
                .quirks
                .archive_after_days,
            90.0
        );
        let c = Config::parse("[quirks]\narchive_after_days = 7\n").unwrap();
        assert_eq!(c.quirks.archive_after_days, 7.0);
        assert!(Config::parse("[quirks]\narchive_days = 7\n").is_err());
    }

    #[test]
    fn intervals_section() {
        // Defaults, with no file and with an empty [intervals].
        for text in ["", "[intervals]\n", DEFAULT_TOML] {
            let c = Config::parse(text).unwrap();
            assert_eq!(c.intervals.quirk_archive_days, Some(90.0));
            assert_eq!(c.intervals.lock_timeout_secs, Some(30.0));
            assert_eq!(c.intervals.watch_poll_secs, Some(30.0));
            assert_eq!(c.intervals.keep_warm_mins, Some(50.0));
            assert_eq!(c.intervals.core_cooldown_secs, Some(0.0));
        }
        // Every key in [intervals] lands where the code reads it.
        let c = Config::parse(
            "[intervals]\nlock_timeout_secs = 1\nlock_lease_secs = 2\nidle_secs = 3\n\
             stale_secs = 4\nwatch_poll_secs = 5\nwake_timeout_secs = 6\n\
             core_cooldown_secs = 7\ncore_start_timeout_secs = 8\n\
             core_exit_timeout_secs = 9\ncore_land_timeout_secs = 10\n\
             keep_warm_mins = 11\nquirk_archive_days = 12\n",
        )
        .unwrap();
        let got = [
            c.lock.timeout_secs,
            c.lock.lease_secs,
            c.awareness.idle_secs,
            c.awareness.stale_secs,
            c.awareness.poll_secs,
            c.wake.timeout_secs,
            c.wake.cooldown_secs,
            c.core.start_timeout_secs,
            c.core.exit_timeout_secs,
            c.core.land_timeout_secs,
            c.core.keep_warm_mins,
            c.quirks.archive_after_days,
        ];
        assert_eq!(got, [1., 2., 3., 4., 5., 6., 7., 8., 9., 10., 11., 12.]);
        assert_eq!(c.lock.timeout(), Duration::from_secs(1));
        assert_eq!(c.awareness.poll(), Duration::from_secs(5));
        // Old places still work.
        let old = Config::parse(
            "[lock]\ntimeout_secs = 3\n[awareness]\npoll_secs = 4\n[wake]\ncooldown_secs = 5\n\
             [core]\nkeep_warm_mins = 6\n[quirks]\narchive_after_days = 7\n",
        )
        .unwrap();
        assert_eq!(old.lock.timeout_secs, 3.0);
        assert_eq!(old.intervals.lock_timeout_secs, Some(3.0));
        assert_eq!(old.awareness.poll_secs, 4.0);
        assert_eq!(old.wake.cooldown_secs, 5.0);
        assert_eq!(old.core.keep_warm_mins, 6.0);
        assert_eq!(old.quirks.archive_after_days, 7.0);
        assert_eq!(old.intervals.quirk_archive_days, Some(7.0));
        // Both present: [intervals] wins, whichever comes first in the file.
        for text in [
            "[quirks]\narchive_after_days = 7\n[intervals]\nquirk_archive_days = 14\n",
            "[intervals]\nquirk_archive_days = 14\n[quirks]\narchive_after_days = 7\n",
        ] {
            let c = Config::parse(text).unwrap();
            assert_eq!(c.quirks.archive_after_days, 14.0);
            assert_eq!(c.intervals.quirk_archive_days, Some(14.0));
        }
        let c =
            Config::parse("[lock]\nlease_secs = 9\n[intervals]\nlock_lease_secs = 1\n").unwrap();
        assert_eq!(c.lock.lease(), Duration::from_secs(1));
        // A typo in [intervals] is an error, like everywhere else.
        assert!(Config::parse("[intervals]\nquirk_archive_day = 1\n").is_err());
        assert!(Config::parse("[intervals]\nkeep_warm_quiet_limit = 1\n").is_err());
    }
}
