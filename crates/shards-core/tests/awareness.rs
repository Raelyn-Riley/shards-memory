// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 03: observation, the board, triggers, dedupe,
//! cooldown and waking the core. All data is fake. No test calls a model:
//! the wake command is a small shell command that records its stdin.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{Duration, Utc};
use shards_core::board::{OverlapKind, State};
use shards_core::sample::{fake_transcript, PLANTED};
use shards_core::watch::EventKind;
use shards_core::{Address, Memory, Provenance, WakeOutcome};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn core(why: &str) -> Provenance {
    Provenance::new("core", Some("Ada".to_string()), why)
}

fn shardp(why: &str) -> Provenance {
    Provenance::new("", None, why)
}

fn addr(s: &str) -> Address {
    Address::parse(s).unwrap()
}

struct World {
    tmp: TempDir,
    mem: Memory,
    /// shards.toml as last given to `config`, before substitution.
    raw: String,
}

impl World {
    fn root(&self) -> PathBuf {
        self.mem.root().to_path_buf()
    }
    fn tx(&self) -> PathBuf {
        self.tmp.path().join("tx")
    }
    fn out(&self) -> PathBuf {
        self.tmp.path().join("out")
    }
    /// Rewrite shards.toml; `{tx}` and `{out}` are replaced by the fixture dirs.
    fn config(&mut self, toml: &str) {
        self.raw = toml.to_string();
        let text = toml
            .replace("{tx}", &self.tx().to_string_lossy())
            .replace("{out}", &self.out().to_string_lossy());
        fs::write(self.root().join("shards.toml"), text).unwrap();
        self.mem = Memory::open_with_env(self.root(), env()).unwrap();
    }
    fn open(&self, id: &str, human: &str) -> Memory {
        let meta = self.mem.shard_open(id, Some(human)).unwrap();
        Memory::open_with_env(&meta.worktree, env()).unwrap()
    }
    /// Files the fake wake command wrote (one per wake), in order.
    fn wakes(&self) -> Vec<serde_json::Value> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.out())
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|p| p.extension().is_some_and(|x| x == "json"));
        files.sort();
        files
            .iter()
            .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
            .collect()
    }
    fn wake_log(&self) -> String {
        fs::read_to_string(self.root().join(".shards/wake.log")).unwrap_or_default()
    }
}

/// Records each wake's stdin to `{out}/wake-<n>.json` and its SHARDS_MEMORY.
const RECORD: &str = r#"mkdir -p {out}; n=$(ls {out} | grep -c json); cat > {out}/wake-$(printf %03d $n).json; printf '%s\n' "$SHARDS_MEMORY" >> {out}/env.txt"#;

fn world(extra: &str) -> World {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &core(""), env()).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    for (a, v) in [
        ("devices/kiln-east/ip", "10.0.4.40"),
        ("devices/kiln-east/port", "8080"),
        ("devices/press-02/ip", "10.0.4.41"),
        ("network/dns", "10.0.0.53"),
    ] {
        mem.set(&addr(a), v, &core("seed")).unwrap();
    }
    let mut w = World {
        tmp,
        mem,
        raw: String::new(),
    };
    let cfg = format!(
        "[activity]\ntranscript_dir = \"{{tx}}\"\n[wake]\ncommand = '''{RECORD}'''\ncooldown_secs = 0\n{extra}"
    );
    w.config(&cfg);
    w
}

fn edit_uncommitted(shard: &Memory, address: &str, value: &str) {
    let p = shard.root().join(addr(address).home());
    let text = fs::read_to_string(&p).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    *lines.last_mut().unwrap() = value;
    fs::write(&p, lines.join("\n") + "\n").unwrap();
}

fn kinds(events: &[shards_core::Event]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind).collect()
}

// (a) two shards change the same fact: same-fact on the board, fires once.
#[test]
fn same_fact_shows_on_the_board_and_fires_once() {
    let w = world("");
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    let s3 = w.open("s3", "Cleo");
    s1.supersede(
        &addr("devices/kiln-east/ip"),
        "10.0.4.50",
        &shardp("rewired"),
    )
    .unwrap();
    edit_uncommitted(&s2, "devices/kiln-east/ip", "10.0.4.60");
    s3.supersede(&addr("network/dns"), "10.0.0.54", &shardp("new resolver"))
        .unwrap();
    let now = Utc::now();

    let b = w.mem.board(now).unwrap();
    assert_eq!(b.shards.len(), 3);
    let row = |id: &str| b.shards.iter().find(|s| s.id == id).unwrap();
    assert_eq!(row("s1").facts_in_flight, 1);
    assert!(!row("s1").in_flight[0].uncommitted);
    assert!(
        row("s2").in_flight[0].uncommitted,
        "worktree edits are in flight"
    );
    assert_eq!(row("s2").human.as_deref(), Some("Basil"));
    let same: Vec<_> = b
        .overlaps
        .iter()
        .filter(|o| o.kind == OverlapKind::SameFact)
        .collect();
    assert_eq!(same.len(), 1, "{:?}", b.overlaps);
    assert_eq!(same[0].target, "devices/kiln-east/ip");
    assert_eq!(same[0].shards, vec!["s1", "s2"]);
    assert_eq!(same[0].humans, vec!["Ada", "Basil"]);
    assert!(b.overlaps.iter().any(|o| o.kind == OverlapKind::SameSection
        && o.target == "devices"
        && o.shards == vec!["s1", "s2"]));

    let r = w.mem.watch_pass(now).unwrap();
    // same_section is off by default.
    assert_eq!(kinds(&r.fired), vec![EventKind::SameFact]);
    let e = &r.fired[0];
    assert_eq!(e.shards, vec!["s1", "s2"]);
    assert_eq!(e.addresses, vec!["devices/kiln-east/ip"]);
    assert_eq!(e.humans, vec!["Ada", "Basil"]);
    assert_eq!(e.key, "same-fact:devices/kiln-east/ip:s1,s2");
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 1, .. }));

    // Still holds: nothing fires, nobody is woken.
    for i in 1..4 {
        let r = w.mem.watch_pass(now + Duration::seconds(i)).unwrap();
        assert!(r.fired.is_empty());
        assert_eq!(r.wake, WakeOutcome::Idle);
    }
    assert_eq!(w.wakes().len(), 1);
    let events = fs::read_to_string(w.root().join(".shards/events.jsonl")).unwrap();
    assert_eq!(events.lines().count(), 1);

    // It clears (s2 puts the value back), then comes back: fires again.
    edit_uncommitted(&s2, "devices/kiln-east/ip", "10.0.4.40");
    assert!(w.mem.watch_pass(now).unwrap().fired.is_empty());
    assert!(w.mem.board(now).unwrap().overlaps.is_empty());
    edit_uncommitted(&s2, "devices/kiln-east/ip", "10.0.4.61");
    let r = w.mem.watch_pass(now).unwrap();
    assert_eq!(kinds(&r.fired), vec![EventKind::SameFact]);
    assert_eq!(r.fired[0].id, 2);
    assert_eq!(w.wakes().len(), 2);
}

#[test]
fn same_section_fires_only_when_switched_on() {
    let mut w = world("");
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    s2.supersede(&addr("devices/press-02/ip"), "10.0.4.51", &shardp("y"))
        .unwrap();
    let now = Utc::now();
    assert!(w.mem.watch_pass(now).unwrap().fired.is_empty());
    let cfg = w.raw.clone();
    w.config(&format!("{cfg}[triggers]\nsame_section = true\n"));
    let r = w.mem.watch_pass(now).unwrap();
    assert_eq!(kinds(&r.fired), vec![EventKind::SameSection]);
    assert_eq!(
        r.fired[0].addresses,
        vec!["devices/kiln-east/ip", "devices/press-02/ip"]
    );
}

// (b) a dead shard with work goes stale and fires.
#[test]
fn a_dead_shard_with_work_goes_stale_and_fires() {
    let w = world("[awareness]\nidle_secs = 600\nstale_secs = 3600\n");
    let s1 = w.open("s1", "Ada");
    let _idle = w.open("s2", "Basil"); // no work: never fires
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    let t0 = Utc::now();
    fake_transcript(&w.tx(), s1.root(), "sess-a", t0 - Duration::minutes(20)).unwrap();

    let b = w.mem.board(t0).unwrap();
    let s1row = &b.shards[0];
    assert_eq!(s1row.state, State::Active);
    assert!(w.mem.watch_pass(t0).unwrap().fired.is_empty());

    let b = w.mem.board(t0 + Duration::minutes(15)).unwrap();
    assert_eq!(b.shards[0].state, State::Idle);

    let later = t0 + Duration::hours(2);
    let b = w.mem.board(later).unwrap();
    assert_eq!(b.shards[0].state, State::Stale);
    assert_eq!(b.shards[1].state, State::Stale);
    let r = w.mem.watch_pass(later).unwrap();
    assert_eq!(kinds(&r.fired), vec![EventKind::StaleWithWork]);
    assert_eq!(r.fired[0].shards, vec!["s1"]);
    assert_eq!(r.fired[0].addresses, vec!["devices/kiln-east/ip"]);
    assert_eq!(r.fired[0].humans, vec!["Ada"]);
    assert!(w
        .mem
        .watch_pass(later + Duration::minutes(5))
        .unwrap()
        .fired
        .is_empty());

    // The shard wakes up (new transcript activity): the condition clears.
    let p = fake_transcript(&w.tx(), s1.root(), "sess-b", later).unwrap();
    assert!(p.exists());
    let b = w.mem.board(later + Duration::minutes(12)).unwrap();
    assert_eq!(b.shards[0].state, State::Active);
    assert_eq!(b.shards[0].last_activity_from, "transcript");
    assert!(w
        .mem
        .watch_pass(later + Duration::minutes(12))
        .unwrap()
        .fired
        .is_empty());
    // ...and goes stale again: fires again.
    let r = w.mem.watch_pass(later + Duration::hours(3)).unwrap();
    assert_eq!(kinds(&r.fired), vec![EventKind::StaleWithWork]);
}

// (c) cooldown holds, then pending events are delivered.
#[test]
fn cooldown_holds_then_pending_events_are_delivered() {
    let mut w = world("");
    let cfg = w
        .raw
        .clone()
        .replace("cooldown_secs = 0", "cooldown_secs = 600");
    w.config(&cfg);
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    let s3 = w.open("s3", "Cleo");
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    s2.supersede(&addr("devices/kiln-east/ip"), "10.0.4.51", &shardp("y"))
        .unwrap();
    let t0 = Utc::now();

    let r = w.mem.watch_pass(t0).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 1, .. }));
    assert_eq!(r.pending, 0);

    // A new condition during the cooldown: recorded, kept, not woken.
    s3.supersede(&addr("network/dns"), "10.0.0.99", &shardp("z"))
        .unwrap();
    s1.supersede(&addr("network/dns"), "10.0.0.98", &shardp("z"))
        .unwrap();
    let r = w.mem.watch_pass(t0 + Duration::seconds(10)).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert_eq!(r.fired[0].addresses, vec!["network/dns"]);
    assert!(
        matches!(r.wake, WakeOutcome::Cooldown { .. }),
        "{:?}",
        r.wake
    );
    assert_eq!(r.pending, 1);
    // Another one still inside the cooldown.
    edit_uncommitted(&s3, "devices/press-02/ip", "10.0.4.70");
    edit_uncommitted(&s2, "devices/press-02/ip", "10.0.4.71");
    let r = w.mem.watch_pass(t0 + Duration::seconds(300)).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert_eq!(r.pending, 2);
    let r = w.mem.watch_pass(t0 + Duration::seconds(599)).unwrap();
    assert!(r.fired.is_empty());
    assert!(matches!(r.wake, WakeOutcome::Cooldown { .. }));
    assert_eq!(w.wakes().len(), 1, "no wake during the cooldown");
    // The pending events are visible to the core in the meantime.
    let ctx = w.mem.core_context(t0 + Duration::seconds(599)).unwrap();
    assert_eq!(ctx.events.len(), 2);

    // Cooldown over: both pending events go in one wake, and only they.
    let r = w.mem.watch_pass(t0 + Duration::seconds(601)).unwrap();
    assert!(r.fired.is_empty());
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 2, .. }));
    assert_eq!(r.pending, 0);
    let wakes = w.wakes();
    assert_eq!(wakes.len(), 2);
    let ids: Vec<u64> = wakes[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![2, 3]);
    assert_eq!(wakes[0].as_array().unwrap()[0]["id"], 1);
    // The command saw SHARDS_MEMORY.
    let env = fs::read_to_string(w.out().join("env.txt")).unwrap();
    assert_eq!(env.lines().next().unwrap(), w.root().to_string_lossy());
    assert!(w.mem.core_context(t0).unwrap().events.is_empty());
}

// (d) a failing wake command leaves events pending and the watcher alive.
#[test]
fn a_failing_or_hanging_wake_leaves_events_pending() {
    let mut w = world("");
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    s2.supersede(&addr("devices/kiln-east/ip"), "10.0.4.51", &shardp("y"))
        .unwrap();
    let base = w.raw.clone();
    let t0 = Utc::now();

    w.config(&base.replace(RECORD, "cat >/dev/null; exit 3"));
    let r = w.mem.watch_pass(t0).unwrap();
    assert_eq!(r.fired.len(), 1);
    match &r.wake {
        WakeOutcome::Failed { events: 1, reason } => assert!(reason.contains("3"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert_eq!(r.pending, 1);
    assert!(w.wake_log().contains("wake FAILED"), "{}", w.wake_log());

    // A hanging command is killed at the timeout; the pass returns.
    let hang = base
        .replace(RECORD, "sleep 30")
        .replace("cooldown_secs = 0", "cooldown_secs = 0\ntimeout_secs = 0.5");
    w.config(&hang);
    let started = Instant::now();
    let r = w.mem.watch_pass(t0 + Duration::seconds(1)).unwrap();
    assert!(started.elapsed().as_secs_f64() < 10.0);
    match &r.wake {
        WakeOutcome::Failed { reason, .. } => assert!(reason.contains("timed out"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert_eq!(r.pending, 1);
    assert!(w.wake_log().contains("timed out"));

    // A command that can't be found fails the same way.
    w.config(&base.replace(RECORD, "/nonexistent/shards-core-wake"));
    let r = w.mem.watch_pass(t0 + Duration::seconds(2)).unwrap();
    assert!(matches!(r.wake, WakeOutcome::Failed { .. }));

    // Fixed command: the same event is delivered.
    w.config(&base);
    let r = w.mem.watch_pass(t0 + Duration::seconds(3)).unwrap();
    assert!(r.fired.is_empty());
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 1, .. }));
    assert_eq!(w.wakes()[0].as_array().unwrap()[0]["id"], 1);
}

#[test]
fn no_wake_command_records_events_only() {
    let mut w = world("");
    let cfg = w.raw.clone().replace(RECORD, "");
    w.config(&cfg);
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    s1.supersede(&addr("network/dns"), "10.0.0.1", &shardp("x"))
        .unwrap();
    edit_uncommitted(&s2, "network/dns", "10.0.0.2");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert_eq!(r.wake, WakeOutcome::NoCommand);
    assert!(!w.out().exists());
    assert_eq!(w.mem.core_context(Utc::now()).unwrap().events.len(), 1);
}

// (e) no transcript at all: the board still works from git.
#[test]
fn without_a_transcript_the_board_works_from_git() {
    for source in ["claude-code", "none"] {
        let mut w = world("");
        w.config(&format!(
            "[activity]\nsource = \"{source}\"\ntranscript_dir = \"{{tx}}/does-not-exist\"\n"
        ));
        let s1 = w.open("s1", "Ada");
        s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
            .unwrap();
        edit_uncommitted(&s1, "network/dns", "10.0.0.9");
        let b = w.mem.board(Utc::now()).unwrap();
        assert_eq!(b.activity_source, source);
        let s = &b.shards[0];
        assert_eq!(s.facts_in_flight, 2);
        assert!(s.recent_actions.is_empty());
        assert_ne!(s.last_activity_from, "transcript");
        assert_eq!(s.state, State::Active);
        w.mem.watch_pass(Utc::now()).unwrap();
    }
}

#[test]
fn transcripts_are_read_incrementally() {
    let w = world("");
    let s1 = w.open("s1", "Ada");
    let t0 = Utc::now() - Duration::minutes(30);
    let path = fake_transcript(&w.tx(), s1.root(), "sess", t0).unwrap();
    let b = w.mem.board(Utc::now()).unwrap();
    // Opened after the transcript's last record: the open time wins.
    assert_eq!(b.shards[0].last_activity_from, "opened");
    let st = w.mem.activity_state("s1");
    let n = st.recent.len();
    assert_eq!(n, 9, "{:?}", st.recent);
    let offset = st.files.values().next().unwrap().offset;
    assert_eq!(offset, fs::metadata(&path).unwrap().len());

    // Nothing new: nothing changes. A partial line is left for later.
    w.mem.board(Utc::now()).unwrap();
    assert_eq!(w.mem.activity_state("s1").recent.len(), n);
    let partial = r#"{"type":"assistant","timestamp":"2026-11-04T23:59:00.000Z","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x/y/later.md"}}]}}"#;
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    use std::io::Write;
    f.write_all(&partial.as_bytes()[..40]).unwrap();
    w.mem.board(Utc::now()).unwrap();
    assert_eq!(w.mem.activity_state("s1").recent.len(), n);
    f.write_all(&partial.as_bytes()[40..]).unwrap();
    f.write_all(b"\nnot json at all\n").unwrap();
    w.mem.board(Utc::now()).unwrap();
    let st = w.mem.activity_state("s1");
    assert_eq!(st.recent.len(), n + 1);
    assert_eq!(st.recent.last().unwrap().summary, "later.md");
    let b = w.mem.board(Utc::now()).unwrap();
    assert_eq!(b.shards[0].recent_actions.len(), 5);
    assert_eq!(b.shards[0].recent_actions[4].tool, "Read");
}

// (f) privacy, at the library level (the CLI test covers printed output).
#[test]
fn planted_secrets_and_prompts_never_reach_state_or_events() {
    let w = world("");
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    edit_uncommitted(&s2, "devices/kiln-east/ip", "10.0.4.51");
    fake_transcript(&w.tx(), s1.root(), "sess", Utc::now() - Duration::hours(3)).unwrap();
    fake_transcript(&w.tx(), s2.root(), "sess", Utc::now() - Duration::hours(3)).unwrap();
    let now = Utc::now() + Duration::hours(2);
    let b = w.mem.board(now).unwrap();
    let r = w.mem.watch_pass(now).unwrap();
    assert!(!r.fired.is_empty());
    let ctx = w.mem.core_context(now).unwrap();
    let mut texts = vec![
        serde_json::to_string(&b).unwrap(),
        serde_json::to_string(&ctx).unwrap(),
        format!("{r:?}"),
    ];
    texts.extend(all_files(&w.root().join(".shards"), "worktrees"));
    texts.extend(all_files(&w.out(), ""));
    for t in &texts {
        for p in PLANTED {
            assert!(!t.contains(p), "`{p}` leaked into:\n{t}");
        }
    }
    // The summaries are there, though.
    let actions = &w.mem.activity_state("s1").recent;
    let tools: Vec<&str> = actions.iter().map(|a| a.tool.as_str()).collect();
    assert!(tools.contains(&"Edit") && tools.contains(&"Bash") && tools.contains(&"Grep"));
    assert!(actions.iter().any(|a| a.summary == "ip.md"));
    assert!(actions.iter().any(|a| a
        .summary
        .starts_with("curl -H \"Authorization: Bearer [REDACTED]\"")));
    assert!(actions.iter().all(|a| a.summary.chars().count() <= 80));
}

fn all_files(dir: &Path, skip: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if !skip.is_empty() && e.file_name() == skip {
            continue;
        }
        if p.is_dir() {
            out.extend(all_files(&p, skip));
        } else if let Ok(bytes) = fs::read(&p) {
            out.push(format!(
                "{}:\n{}",
                p.display(),
                String::from_utf8_lossy(&bytes)
            ));
        }
    }
    out
}

// (h) a restart doesn't re-fire.
#[test]
fn a_restart_does_not_refire_and_keeps_cooldown_and_pending() {
    let mut w = world("");
    let cfg = w
        .raw
        .clone()
        .replace("cooldown_secs = 0", "cooldown_secs = 600");
    w.config(&cfg);
    let s1 = w.open("s1", "Ada");
    let s2 = w.open("s2", "Basil");
    s1.supersede(&addr("devices/kiln-east/ip"), "10.0.4.50", &shardp("x"))
        .unwrap();
    s2.supersede(&addr("devices/kiln-east/ip"), "10.0.4.51", &shardp("y"))
        .unwrap();
    let t0 = Utc::now();
    let r = w.mem.watch_pass(t0).unwrap();
    assert_eq!(r.fired.len(), 1);
    s2.supersede(&addr("network/dns"), "10.0.0.7", &shardp("y"))
        .unwrap();
    s1.supersede(&addr("network/dns"), "10.0.0.8", &shardp("y"))
        .unwrap();
    let r = w.mem.watch_pass(t0 + Duration::seconds(5)).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert_eq!(r.pending, 1);

    // "Restart": a brand-new Memory, as a new process would open.
    let fresh = Memory::open_with_env(w.root(), env()).unwrap();
    let r = fresh.watch_pass(t0 + Duration::seconds(10)).unwrap();
    assert!(r.fired.is_empty(), "restart re-fired {:?}", r.fired);
    assert!(matches!(r.wake, WakeOutcome::Cooldown { .. }));
    assert_eq!(r.pending, 1);
    assert_eq!(w.wakes().len(), 1);
    let r = fresh.watch_pass(t0 + Duration::seconds(700)).unwrap();
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 1, .. }));

    // A corrupt state file is moved aside and logged, not fatal.
    fs::write(w.root().join(".shards/watch.json"), "{nope").unwrap();
    fresh.watch_pass(t0 + Duration::seconds(2000)).unwrap();
    assert!(w.root().join(".shards/watch.json.bad").exists());
    assert!(w.wake_log().contains("unreadable"));
}

#[test]
fn open_conflicts_and_soul_proposals_fire_once() {
    let w = world("");
    let s1 = w.open("s1", "Basil");
    s1.supersede(&addr("devices/kiln-east/port"), "9090", &shardp("x"))
        .unwrap();
    s1.put_file("soul/voice.md", "Speak plainly.\n", &shardp("tone"))
        .unwrap();
    w.mem
        .supersede(&addr("devices/kiln-east/port"), "7070", &core("main moved"))
        .unwrap();
    let rep = w.mem.dinit("s1", false).unwrap();
    assert_eq!(rep.conflicts.len(), 1);
    let now = Utc::now();
    let b = w.mem.board(now).unwrap();
    assert_eq!(b.conflicts.len(), 1);
    assert_eq!(b.conflicts[0].target, "devices/kiln-east/port");
    assert_eq!(b.conflicts[0].humans, vec!["Ada", "Basil"]);
    assert_eq!(b.soul_proposals.len(), 1);
    assert_eq!(b.soul_proposals[0].paths, vec!["soul/voice.md"]);
    let r = w.mem.watch_pass(now).unwrap();
    let mut k = kinds(&r.fired);
    k.sort();
    assert_eq!(k, vec![EventKind::OpenConflict, EventKind::SoulProposal]);
    assert!(w.mem.watch_pass(now).unwrap().fired.is_empty());
}

#[test]
fn closed_shards_leave_the_board_and_their_activity_state() {
    let w = world("");
    let s1 = w.open("s1", "Ada");
    s1.supersede(&addr("network/dns"), "10.0.0.2", &shardp("x"))
        .unwrap();
    fake_transcript(&w.tx(), s1.root(), "sess", Utc::now()).unwrap();
    w.mem.board(Utc::now()).unwrap();
    assert!(w.root().join(".shards/activity/s1.json").exists());
    assert!(w.mem.dinit("s1", false).unwrap().closed);
    assert!(w.mem.board(Utc::now()).unwrap().shards.is_empty());
    assert!(!w.root().join(".shards/activity/s1.json").exists());
}
