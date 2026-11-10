// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 14, her rulings: newer information about a fact on main fixes
//! what was waiting on it (superseded = fixed, logged and linked), and the
//! core checks before it decides. All data here is fake.

use chrono::Utc;
use shards_core::{
    Address, ChangeStatus, Memory, Provenance, QuestionStatus, Take, Verdict, CHECK_RULE,
};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn core(why: &str) -> Provenance {
    Provenance::new("core", None, why)
}

fn shardp(why: &str) -> Provenance {
    Provenance::new("", None, why)
}

fn addr(s: &str) -> Address {
    Address::parse(s).unwrap()
}

fn fresh() -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &core(""), env()).unwrap();
    std::fs::write(root.join("shards.toml"), "[activity]\nsource = \"none\"\n").unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set(&addr("mill/three/spindle"), "original", &core("seed"))
        .unwrap();
    mem.set(&addr("mill/three/coolant"), "flood", &core("seed"))
        .unwrap();
    (tmp, mem)
}

fn open_shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    Memory::open_with_env(&meta.worktree, env()).unwrap()
}

/// Theo's shard says the spindle was swapped; the core holds it (p1#1, ask1).
fn held_spindle(mem: &Memory) {
    let s = open_shard(mem, "s1", "Theo");
    s.supersede(
        &addr("mill/three/spindle"),
        "swapped 2026-11",
        &shardp("Theo said it in passing"),
    )
    .unwrap();
    mem.dinit("s1", false).unwrap();
    mem.proposal_decide(
        "p1",
        Some(1),
        &Verdict::Hold("Theo mentioned the spindle was swapped. Would you confirm it?".into()),
        &core("only said in passing"),
    )
    .unwrap();
    assert!(mem.question("ask1").unwrap().is_open());
}

fn assert_fixed_by(mem: &Memory, by: &str) {
    let q = mem.question("ask1").unwrap();
    assert_eq!(q.status, QuestionStatus::Closed);
    let c = q.closed.as_ref().unwrap();
    assert_eq!(c.action, "fixed");
    assert_eq!(c.by_change, by);
    assert_eq!(q.log.last().unwrap().kind, "fixed");
    assert_eq!(q.log.last().unwrap().change.as_deref(), Some(by));
    let p = mem.proposal("p1").unwrap();
    let ch = p.change(1).unwrap();
    assert_eq!(ch.status, ChangeStatus::Fixed);
    assert_eq!(ch.fixed_by.as_deref(), Some(by));
    let d = p.log.last().unwrap();
    assert_eq!(d.action, "fixed");
    assert_eq!(d.question.as_deref(), Some("ask1"));
    assert!(!p.is_open());
    // Closed questions are no longer asked.
    assert!(mem.questions_for(Some("Rosa")).unwrap().is_empty());
}

#[test]
fn an_accepted_change_from_anyone_fixes_the_held_change_and_closes_its_question() {
    let (_t, mem) = fresh();
    held_spindle(&mem);
    // Rosa's shard, not answering the question, writes the spindle anew.
    let s2 = open_shard(&mem, "s2", "Rosa");
    s2.supersede(
        &addr("mill/three/spindle"),
        "new cartridge, 2026-11-08",
        &shardp("Rosa fitted it and read the serial off it"),
    )
    .unwrap();
    mem.dinit("s2", false).unwrap();
    mem.proposal_decide(
        "p2",
        None,
        &Verdict::Accept(None),
        &core("serial matches the invoice"),
    )
    .unwrap();
    assert_fixed_by(&mem, "p2#1");
    let q = mem.question("ask1").unwrap();
    assert_eq!(
        q.closed.unwrap().value.as_deref(),
        Some("new cartridge, 2026-11-08")
    );
    // One commit: the fact, p2, p1 and ask1 together.
    let head = mem.head().unwrap().unwrap();
    let paths = mem.changed_paths(&head).unwrap();
    for p in [
        "facts/mill/three/spindle.md",
        "proposals/p1.json",
        "proposals/p2.json",
        "questions/ask1.json",
    ] {
        assert!(paths.contains(&p.to_string()), "{p} not in {paths:?}");
    }
}

#[test]
fn a_write_on_main_fixes_it_too() {
    let (_t, mem) = fresh();
    held_spindle(&mem);
    mem.supersede(
        &addr("mill/three/spindle"),
        "swapped, confirmed",
        &Provenance::new("core", Some("Rosa".into()), "Rosa read the serial"),
    )
    .unwrap();
    assert_fixed_by(&mem, "supersede mill/three/spindle on main by core / Rosa");
    // A write to another fact leaves an open question alone.
    held_spindle_again(&mem);
    mem.supersede(&addr("mill/three/coolant"), "mist", &core("pump label"))
        .unwrap();
    assert!(mem.question("ask2").unwrap().is_open());
}

/// A second hold on the spindle (p2#1, ask2), after ask1 closed.
fn held_spindle_again(mem: &Memory) {
    let s = open_shard(mem, "s9", "Theo");
    s.supersede(
        &addr("mill/three/spindle"),
        "swapped twice",
        &shardp("Theo again"),
    )
    .unwrap();
    mem.dinit("s9", false).unwrap();
    mem.proposal_decide(
        "p2",
        Some(1),
        &Verdict::Hold("Swapped twice? Confirm?".into()),
        &core("odd"),
    )
    .unwrap();
}

#[test]
fn a_settled_conflict_closes_an_open_question_as_fixed_by_the_incident() {
    let (_t, mem) = fresh();
    held_spindle(&mem); // p1#1 held, ask1 open
                        // Two more shards conflict over the spindle.
    let a = open_shard(&mem, "s2", "Rosa");
    let b = open_shard(&mem, "s3", "Iggy");
    a.supersede(&addr("mill/three/coolant"), "mist", &shardp("Rosa"))
        .unwrap();
    b.supersede(&addr("mill/three/coolant"), "dry", &shardp("Iggy"))
        .unwrap();
    // A held coolant change from s2 with its own question (ask2).
    mem.dinit("s2", false).unwrap();
    mem.proposal_decide(
        "p2",
        None,
        &Verdict::Hold("Rosa says mist. Confirm?".into()),
        &core("unsure"),
    )
    .unwrap();
    // s3's coolant reaches main only through a conflict: set main first.
    let c = open_shard(&mem, "s4", "Juno");
    c.supersede(&addr("mill/three/coolant"), "flood-2", &shardp("Juno"))
        .unwrap();
    mem.dinit("s4", false).unwrap();
    mem.dinit("s3", false).unwrap();
    // p3 is s4's (Juno), p4 is s3's (Iggy). Accepting p3 fixes ask2 ...
    mem.proposal_decide(
        "p3",
        None,
        &Verdict::Accept(None),
        &core("pump label says flood"),
    )
    .unwrap();
    let q2 = mem.question("ask2").unwrap();
    assert_eq!(q2.closed.as_ref().unwrap().by_change, "p3#1");
    // ... and p4's accept conflicts; Theo settles it: incident i1.
    mem.proposal_decide("p4", None, &Verdict::Accept(None), &core("looks right"))
        .unwrap();
    assert_eq!(
        mem.proposal("p4").unwrap().change(1).unwrap().status,
        ChangeStatus::Conflict
    );
    // Meanwhile Iggy's shard re-proposes coolant and it is held (ask3) ...
    let d = open_shard(&mem, "s5", "Iggy");
    d.supersede(&addr("mill/three/coolant"), "dry-2", &shardp("Iggy again"))
        .unwrap();
    mem.dinit("s5", false).unwrap();
    mem.proposal_decide(
        "p5",
        None,
        &Verdict::Hold("Iggy says dry. Confirm?".into()),
        &core("unsure"),
    )
    .unwrap();
    assert!(mem.question("ask3").unwrap().is_open());
    // ... and the person's settlement closes ask3, linked to the incident.
    let r = mem
        .resolve_conflict(
            "mill/three/coolant",
            &Take::Main,
            None,
            &Provenance::new("", Some("Theo".into()), "read the pump label: flood-2"),
        )
        .unwrap();
    let q3 = mem.question("ask3").unwrap();
    let closed = q3.closed.unwrap();
    assert_eq!(closed.action, "fixed");
    assert_eq!(closed.by_change, r.incident);
    assert_eq!(closed.value.as_deref(), Some("flood-2"));
    let p5 = mem.proposal("p5").unwrap();
    assert_eq!(p5.change(1).unwrap().status, ChangeStatus::Fixed);
    assert_eq!(
        p5.change(1).unwrap().fixed_by.as_deref(),
        Some(r.incident.as_str())
    );
    // The unrelated spindle question stays open.
    assert!(mem.question("ask1").unwrap().is_open());
}

#[test]
fn the_core_is_told_to_check_and_a_rejected_answer_reaches_the_next_person() {
    let (_t, mem) = fresh();
    let ctx = mem.core_context(Utc::now()).unwrap();
    assert!(ctx.rule.contains("harmless check"), "{}", ctx.rule);
    assert!(ctx
        .rule
        .contains("never anything that writes, moves or actuates"));
    let json = serde_json::to_value(&ctx).unwrap();
    assert_eq!(json["rule"], CHECK_RULE);
    let soul = mem.read_file_at("HEAD", "soul/core.md").unwrap().unwrap();
    assert!(soul.contains("Check first."), "{soul}");

    held_spindle(&mem);
    // Rosa answers; the core's check disagrees and it rejects with the result.
    let s2 = open_shard(&mem, "s2", "Rosa");
    let mut p = shardp("Rosa says yes, swapped");
    p.answers = Some("ask1".into());
    s2.supersede(&addr("mill/three/spindle"), "swapped 2026-11", &p)
        .unwrap();
    mem.dinit("s2", false).unwrap();
    mem.proposal_decide(
        "p2",
        None,
        &Verdict::Reject,
        &core("checked: the controller still reports the old spindle serial"),
    )
    .unwrap();
    let q = mem.question("ask1").unwrap();
    assert!(q.is_open());
    let brief = mem.questions_brief(Some("Iggy")).unwrap();
    assert!(
        brief.contains("an earlier answer (p2#1) was rejected by core: checked: the controller still reports the old spindle serial"),
        "{brief}"
    );
    // A rejected answer is not newer information: p1#1 is still held.
    assert_eq!(
        mem.proposal("p1").unwrap().change(1).unwrap().status,
        ChangeStatus::Held
    );
}
