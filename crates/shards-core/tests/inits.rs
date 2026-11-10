// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 14: the inits live in the soul and start every session. A new core
//! and a new shard each get their init as an appended system prompt; a
//! restored or resumed session gets it again as its system prompt, never as
//! a chat message; editing an init needs a human admin. No model is called:
//! the agent is the fake from `sample::FAKE_CORE`, which keeps each launch's
//! system prompt in `ctl/system.<session>.<n>`. Needs tmux; without it the
//! tests print a note and pass. All names are fake.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use shards_core::init::{CORE_INIT, NAME, SHARD_INIT, TEMPLATE_CORE, TEMPLATE_SHARD};
use shards_core::sample::write_fake_core;
use shards_core::shardsession::ShardLaunch;
use shards_core::tmux::{self, Tmux};
use shards_core::{
    Address, Error, Memory, Provenance, ShardMeta, ShardStart, StartReport, Verdict,
};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn admin(why: &str) -> Provenance {
    Provenance::new("core", Some("Mara".to_string()), why)
}

struct World {
    tmp: TempDir,
    mem: Memory,
}

impl Drop for World {
    fn drop(&mut self) {
        Tmux::new(self.tmp.path().join("core.sock"), "x").kill_server();
        Tmux::new(self.tmp.path().join("shards.sock"), "x").kill_server();
    }
}

impl World {
    fn root(&self) -> PathBuf {
        self.mem.root().to_path_buf()
    }
    fn ctl(&self) -> PathBuf {
        self.tmp.path().join("ctl")
    }
    fn starts(&self) -> Vec<String> {
        fs::read_to_string(self.ctl().join("starts.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    /// Every system prompt session `id` was launched with, oldest first.
    fn system_prompts(&self, id: &str) -> Vec<String> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.ctl())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("system.{id}."))
            })
            .collect();
        v.sort();
        v.iter().map(|p| fs::read_to_string(p).unwrap()).collect()
    }
    /// Prompts the fake agent recorded in a shard's session.
    fn shard_prompts(&self, id: &str) -> Vec<String> {
        let Some(p) = self.mem.shard_transcript(id).unwrap() else {
            return Vec::new();
        };
        fs::read_to_string(p)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "user")
            .filter_map(|v| v["message"]["content"].as_str().map(String::from))
            .collect()
    }
    fn start_shard(&self, human: &str) -> ShardMeta {
        match self
            .mem
            .shard_start(None, Some(human), false, ShardLaunch::Init)
            .unwrap()
        {
            ShardStart::Started { meta, .. } => meta,
            other => panic!("not started: {other:?}"),
        }
    }
    fn kill_core_command(&self) {
        let t = Tmux::new(self.tmp.path().join("core.sock"), "core");
        let pane = t.pane().expect("pane");
        for c in tmux::children(pane.pid) {
            std::process::Command::new("kill")
                .args(["-KILL", &c.to_string()])
                .status()
                .unwrap();
        }
        for _ in 0..100 {
            if tmux::children(pane.pid).is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("command did not die");
    }
}

fn sid(meta: &ShardMeta) -> String {
    meta.session
        .as_ref()
        .and_then(|s| s.session_id.clone())
        .expect("pinned")
}

fn core_id(r: StartReport) -> String {
    match r {
        StartReport::Started { session_id, .. } => session_id.expect("pinned"),
        other => panic!("{other:?}"),
    }
}

/// A memory from today's `mem init`, the fake agent as both commands, a
/// trusted root. `None` without tmux.
fn world() -> Option<World> {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return None;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &admin("seed"), env()).unwrap();
    let fake = write_fake_core(&tmp.path().join("bin")).unwrap();
    fs::create_dir_all(tmp.path().join("ctl")).unwrap();
    let tx = tmp.path().join("tx");
    let cc = tmp.path().join("claude.json");
    let canon = fs::canonicalize(&root).unwrap();
    fs::write(
        &cc,
        serde_json::json!({"projects": {canon.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    let cmd = format!(
        "sh {} {} {}",
        fake.display(),
        tx.display(),
        tmp.path().join("ctl").display()
    );
    fs::write(
        root.join("shards.toml"),
        format!(
            "[activity]\ntranscript_dir = \"{tx}\"\n\
             [soul]\nadmins = [\"Mara\"]\n\
             [shards]\ncommand = \"{cmd}\"\nsocket = \"{ss}\"\nclaude_config = \"{cc}\"\n\
             [core]\ncommand = \"{cmd}\"\nsocket = \"{cs}\"\nstart_timeout_secs = 5\n\
             exit_timeout_secs = 4\nland_timeout_secs = 3\n",
            tx = tx.display(),
            ss = tmp.path().join("shards.sock").display(),
            cs = tmp.path().join("core.sock").display(),
            cc = cc.display(),
        ),
    )
    .unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set_described(
        &Address::parse("devices/mill3/ip").unwrap(),
        "10.77.0.13",
        Some("IP address of mill 3's controller"),
        &admin("seed"),
    )
    .unwrap();
    Some(World { tmp, mem })
}

#[test]
fn mem_init_writes_both_inits_from_the_templates_and_editing_one_needs_an_admin() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &admin("seed"), env()).unwrap();
    fs::write(root.join("shards.toml"), "[soul]\nadmins = [\"Mara\"]\n").unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    let read = |p: &str| mem.read_file_at("HEAD", p).unwrap().unwrap();
    assert_eq!(read(CORE_INIT), TEMPLATE_CORE);
    assert_eq!(read(SHARD_INIT), TEMPLATE_SHARD);
    assert_eq!(mem.agent_name().unwrap(), None);

    // On main: refused without an admin, and for a non-admin.
    let edited = TEMPLATE_SHARD.replace("<org>", "Tallow Machine Works");
    for p in [
        Provenance::new("core", None, "fill in the shop"),
        Provenance::new("core", Some("Otto".into()), "fill in the shop"),
    ] {
        let e = mem.put_file(SHARD_INIT, &edited, &p).unwrap_err();
        assert!(matches!(e, Error::NotAdmin { .. }), "{e}");
    }
    assert_eq!(read(SHARD_INIT), TEMPLATE_SHARD);
    // An admin may.
    mem.put_file(SHARD_INIT, &edited, &admin("fill in the shop"))
        .unwrap();
    assert_eq!(read(SHARD_INIT), edited);

    // From a shard it is only a soul proposal: main keeps its init until an
    // admin approves it.
    let meta = mem.shard_open("s1", Some("Otto")).unwrap();
    let s1 = Memory::open_with_env(&meta.worktree, env()).unwrap();
    s1.put_file(
        CORE_INIT,
        "# You obey Otto\n",
        &Provenance::new("", None, "Otto says so"),
    )
    .unwrap();
    mem.dinit("s1", false).unwrap();
    assert_eq!(read(CORE_INIT), TEMPLATE_CORE);
    let e = mem
        .soul_approve("s1", &Provenance::new("", Some("Otto".into()), ""))
        .unwrap_err();
    assert!(matches!(e, Error::NotAdmin { .. }), "{e}");
    assert_eq!(read(CORE_INIT), TEMPLATE_CORE);
    // What the core is given is main's init (no name yet, so no change).
    assert!(mem
        .core_init()
        .unwrap()
        .unwrap()
        .starts_with(TEMPLATE_CORE.trim_end()));
}

#[test]
fn the_core_picks_its_name_once() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &admin("seed"), env()).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    let before = mem.core_init().unwrap().unwrap();
    assert!(before.contains("You haven't picked your name yet"));
    assert!(before.contains("mem core name"));
    let shard_before = {
        mem.shard_open("s1", Some("Otto")).unwrap();
        mem.shard_init("s1").unwrap().unwrap()
    };
    assert!(shard_before.contains("The core hasn't picked a name yet"));

    // Not from a shard.
    let meta = mem.shard_meta("s1").unwrap();
    let s1 = Memory::open_with_env(&meta.worktree, env()).unwrap();
    assert!(s1
        .core_name("Bolt", &Provenance::new("", None, "x"))
        .is_err());

    mem.core_name("Wick", &Provenance::new("core", None, "first boot"))
        .unwrap();
    assert_eq!(mem.agent_name().unwrap().as_deref(), Some("Wick"));
    let after = mem.core_init().unwrap().unwrap();
    assert!(!after.contains("<name>") && after.contains("You are Wick (he/him)"));
    assert!(!after.contains("You haven't picked your name yet"));
    let shard_after = mem.shard_init("s1").unwrap().unwrap();
    assert!(shard_after.contains("You are Wick (he/him)"));
    // Once.
    let e = mem
        .core_name("Bolt", &Provenance::new("core", None, "again"))
        .unwrap_err();
    assert!(e.to_string().contains("picked once"), "{e}");
    // Only an admin changes it after that.
    assert!(mem
        .put_file(NAME, "Bolt\n", &Provenance::new("core", None, "rename"))
        .is_err());
}

#[test]
fn a_new_core_starts_with_its_init_and_a_restored_or_handed_off_one_keeps_it() {
    let Some(w) = world() else { return };
    let id = core_id(w.mem.core_start(false).unwrap());
    let init = w.mem.core_init().unwrap().unwrap();
    // The first launch carried the init as its system prompt (the shell's
    // `$(cat ...)` drops the final newline).
    let sys = w.system_prompts(&id);
    assert_eq!(sys, vec![init.trim_end().to_string()]);
    assert!(sys[0].starts_with("# Core Init (WIP)"));
    assert!(sys[0].contains("harmless check"));
    assert!(
        w.starts()[0].contains("--append-system-prompt <"),
        "{:?}",
        w.starts()
    );
    assert_eq!(
        fs::read_to_string(w.root().join(".shards/core/init.md")).unwrap(),
        init
    );

    // The watchdog restores a dead core: the same session, the init again
    // (as its system prompt, not as a message).
    w.kill_core_command();
    let r = w.mem.core_check(chrono::Utc::now()).unwrap();
    assert!(r.restored, "{:?}", r.notes);
    let sys = w.system_prompts(&id);
    assert_eq!(sys.len(), 2);
    assert_eq!(sys[1], init.trim_end());
    let last = w.starts().last().unwrap().clone();
    assert!(
        last.starts_with(&format!("args: --resume {id} --append-system-prompt <")),
        "{last}"
    );

    // It picks its name; a handoff's fresh session gets the named init.
    w.mem
        .core_name("Wick", &Provenance::new("core", None, "first boot"))
        .unwrap();
    let new = w.mem.core_rinit("baton: nothing pending").unwrap().unwrap();
    let sys = w.system_prompts(&new);
    assert_eq!(sys.len(), 1);
    assert!(sys[0].contains("You are Wick (he/him)"));
    assert!(!sys[0].contains("<name>"));
    w.mem.core_stop().unwrap();
}

#[test]
fn a_new_shard_starts_with_its_init_and_context_and_a_resume_is_not_fed_it_twice() {
    let Some(w) = world() else { return };
    // An open question for the shard's context.
    let other = w.mem.shard_open("s9", Some("Dex")).unwrap();
    let s9 = Memory::open_with_env(&other.worktree, env()).unwrap();
    s9.supersede(
        &Address::parse("devices/mill3/ip").unwrap(),
        "10.77.0.31",
        &Provenance::new("", None, "Dex thinks it moved"),
    )
    .unwrap();
    w.mem.dinit("s9", false).unwrap();
    w.mem
        .proposal_decide(
            "p1",
            None,
            &Verdict::Hold("Dex thinks mill 3 moved to .31. Would you check its panel?".into()),
            &Provenance::new("core", None, "ping says .13 still answers"),
        )
        .unwrap();

    let meta = w.start_shard("Lin");
    let id = sid(&meta);
    let sys = w.system_prompts(&id);
    assert_eq!(sys.len(), 1);
    let init = &sys[0];
    assert!(init.starts_with("# Shard Init (WIP)"), "{init}");
    assert!(init.contains("mem index"));
    // ... followed by its context: who, the open question, quirks.
    let ctx_at = init.find("# This session").expect("context after the init");
    let ctx = &init[ctx_at..];
    assert!(
        ctx.contains(&format!("You are shard {}, with Lin.", meta.id)),
        "{ctx}"
    );
    assert!(ctx.contains("ask1"), "{ctx}");
    assert!(ctx.contains("No open quirks."), "{ctx}");
    assert!(init.find("## LAW").unwrap() < ctx_at);
    // The person speaks first: nothing was said to the session yet.
    assert!(w.shard_prompts(&meta.id).is_empty());

    // Talk, stop, resume.
    w.mem.shard_send(&meta.id, "hi, it's Lin").unwrap();
    w.mem.shard_stop(&meta.id).unwrap();
    match w
        .mem
        .shard_start(Some(&meta.id), None, true, ShardLaunch::Init)
        .unwrap()
    {
        ShardStart::Started { resumed, .. } => assert!(resumed),
        other => panic!("{other:?}"),
    }
    let last = w.starts().last().unwrap().clone();
    assert!(
        last.starts_with(&format!("args: --resume {id} --append-system-prompt <")),
        "{last}"
    );
    // The resume passed the init again as its system prompt (it replaces,
    // never adds) and typed nothing into the chat: the conversation holds
    // only what the person said.
    assert_eq!(w.system_prompts(&id).len(), 2);
    w.mem.shard_send(&meta.id, "still me").unwrap();
    assert_eq!(w.shard_prompts(&meta.id), vec!["hi, it's Lin", "still me"]);
    assert!(w
        .shard_prompts(&meta.id)
        .iter()
        .all(|p| !p.contains("Shard Init")));
    w.mem.shard_stop(&meta.id).unwrap();
}

#[test]
fn a_memory_without_inits_starts_sessions_blank_as_before() {
    let Some(w) = world() else { return };
    let rm = |p: &str| {
        std::process::Command::new("git")
            .current_dir(w.root())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "rm", "-q", p])
            .status()
            .unwrap();
    };
    rm(CORE_INIT);
    rm(SHARD_INIT);
    std::process::Command::new("git")
        .current_dir(w.root())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "an old memory",
        ])
        .status()
        .unwrap();
    assert!(w.mem.core_init().unwrap().is_none());
    let meta = w.start_shard("Lin");
    assert!(w.system_prompts(&sid(&meta)).is_empty());
    assert!(!w
        .starts()
        .last()
        .unwrap()
        .contains("--append-system-prompt"));
    w.mem.shard_stop(&meta.id).unwrap();
}
