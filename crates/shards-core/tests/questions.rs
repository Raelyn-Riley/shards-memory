// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 13: a person's conflict pick writes an incident; a held change
//! becomes an open question for the next person; a proposal keeps the
//! shard's drafts. All data here is fake.

use chrono::Utc;
use shards_core::watch::EventKind;
use shards_core::{
    Address, ChangeStatus, Error, Memory, Provenance, QuestionStatus, Take, Verdict,
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

fn answering(why: &str, q: &str) -> Provenance {
    let mut p = shardp(why);
    p.answers = Some(q.to_string());
    p
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
    mem.set(&addr("lathe/two/spindle"), "original", &core("seed"))
        .unwrap();
    mem.set(&addr("kiln/east/temp"), "900", &core("seed"))
        .unwrap();
    (tmp, mem)
}

fn open_shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    Memory::open_with_env(&meta.worktree, env()).unwrap()
}

fn val(mem: &Memory, a: &str) -> Option<String> {
    mem.get(&addr(a)).ok().map(|f| f.value)
}

/// Mike's shard proposes the spindle; the core holds it with an ask.
fn held_spindle(mem: &Memory) {
    let s1 = open_shard(mem, "s1", "Mike");
    s1.supersede(
        &addr("lathe/two/spindle"),
        "replaced 2026-11",
        &shardp("Mike said in passing he replaced it"),
    )
    .unwrap();
    mem.dinit("s1", false).unwrap();
    mem.proposal_decide(
        "p1",
        Some(1),
        &Verdict::Hold(
            "Mike mentioned he replaced the spindle, but only in passing. Would you confirm it?"
                .into(),
        ),
        &core("only said in passing; a person should confirm"),
    )
    .unwrap();
}

#[test]
fn a_resolution_writes_an_incident_with_both_sides_and_wakes_the_core() {
    let (_t, mem) = fresh();
    let a = open_shard(&mem, "s1", "Ada");
    let b = open_shard(&mem, "s2", "Basil");
    a.supersede(&addr("kiln/east/temp"), "940", &shardp("Ada's probe"))
        .unwrap();
    b.supersede(&addr("kiln/east/temp"), "960", &shardp("Basil's probe"))
        .unwrap();
    mem.dinit("s1", false).unwrap();
    mem.dinit("s2", false).unwrap();
    mem.proposal_decide("p1", None, &Verdict::Accept(None), &core("probe ok"))
        .unwrap();
    // s2's accept finds main moved: a conflict.
    mem.proposal_decide("p2", None, &Verdict::Accept(None), &core("probe ok"))
        .unwrap();
    mem.watch_pass(Utc::now()).unwrap();

    // The settler's name and a reason are required.
    let e = mem
        .resolve_conflict("kiln/east/temp", &Take::Shard, None, &core("checked"))
        .unwrap_err();
    assert!(matches!(e, Error::MissingSettler), "{e}");
    let e = mem
        .resolve_conflict(
            "kiln/east/temp",
            &Take::Shard,
            None,
            &Provenance::new("", Some("Cleo".into()), ""),
        )
        .unwrap_err();
    assert!(matches!(e, Error::MissingWhy), "{e}");

    // Cleo checks the dial: Basil's shard was right. Final, no review.
    let r = mem
        .resolve_conflict(
            "kiln/east/temp",
            &Take::Shard,
            None,
            &Provenance::new("", Some("Cleo".into()), "read the dial myself: 960"),
        )
        .unwrap();
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("960"));
    assert_eq!(r.incident, "i1");
    let i = mem.incident("i1").unwrap();
    assert_eq!(
        i.summary,
        "two shards conflicted over kiln/east/temp. Cleo checked it. shard s2 was correct."
    );
    assert_eq!(i.main.value, "940");
    assert_eq!(i.main.why, "Ada's probe");
    assert_eq!(i.main.author, "s1");
    assert_eq!(i.shard_side.value, "960");
    assert_eq!(i.shard_side.why, "Basil's probe");
    assert_eq!(i.shards, vec!["s1", "s2"]);
    assert_eq!(i.humans, vec!["Ada", "Basil", "Cleo"]);
    assert_eq!(i.settled.human, "Cleo");
    assert_eq!(i.settled.why, "read the dial myself: 960");
    assert_eq!(i.settled.take, "shard");
    assert!(!i.settled.at.is_empty() && !i.conflict_recorded.is_empty());
    assert!(mem.root().join("incidents/i1.json").exists());

    // The core is woken for it, the board lists it.
    let r = mem.watch_pass(Utc::now()).unwrap();
    let ev: Vec<_> = r
        .fired
        .iter()
        .filter(|e| e.kind == EventKind::Incident)
        .collect();
    assert_eq!(ev.len(), 1, "{:?}", r.fired);
    assert!(
        ev[0].summary.contains("Cleo checked it"),
        "{}",
        ev[0].summary
    );
    assert!(mem.watch_pass(Utc::now()).unwrap().fired.is_empty());
    let b = mem.board(Utc::now()).unwrap();
    assert_eq!(b.incidents.len(), 1);
    assert_eq!(b.incidents[0].settled_by, "Cleo");
    let ctx = mem.core_context(Utc::now()).unwrap();
    assert_eq!(ctx.board.incidents[0].id, "i1");

    // The core adds a note with a reason; nothing is removed.
    let e = mem
        .incident_note("i1", "probe drift", &core(""))
        .unwrap_err();
    assert!(matches!(e, Error::MissingWhy), "{e}");
    mem.incident_note(
        "i1",
        "Ada's probe reads 20 low; worth a calibration.",
        &core("two conflicts on this kiln this month"),
    )
    .unwrap();
    let after = mem.incident("i1").unwrap();
    assert_eq!(after.notes.len(), 1);
    assert_eq!(after.summary, i.summary);
    assert_eq!(after.settled, i.settled);
    // A shard can't write the core's note.
    let s3 = open_shard(&mem, "s3", "Dee");
    assert!(s3.incident_note("i1", "x", &shardp("y")).is_err());
}

#[test]
fn a_hold_without_an_ask_is_refused() {
    let (_t, mem) = fresh();
    let s1 = open_shard(&mem, "s1", "Mike");
    s1.supersede(&addr("lathe/two/spindle"), "new", &shardp("he said"))
        .unwrap();
    mem.dinit("s1", false).unwrap();
    let e = mem
        .proposal_decide("p1", Some(1), &Verdict::Hold(" ".into()), &core("unsure"))
        .unwrap_err();
    assert!(e.to_string().contains("--ask"), "{e}");
    assert_eq!(
        mem.proposal("p1").unwrap().changes[0].status,
        ChangeStatus::Pending
    );
    assert!(mem.questions().unwrap().is_empty());
}

#[test]
fn a_hold_asks_and_an_accepted_answer_closes_the_question_and_the_held_change() {
    let (_t, mem) = fresh();
    held_spindle(&mem);
    let q = mem.question("ask1").unwrap();
    assert_eq!(q.status, QuestionStatus::Open);
    assert_eq!(q.held_change(), "p1#1");
    assert_eq!(q.said_by_human.as_deref(), Some("Mike"));
    assert_eq!(q.said_why, "Mike said in passing he replaced it");
    assert_eq!(q.held_why, "only said in passing; a person should confirm");
    let p1 = mem.proposal("p1").unwrap();
    assert_eq!(p1.changes[0].question.as_deref(), Some("ask1"));
    assert_eq!(p1.log[0].question.as_deref(), Some("ask1"));
    assert_eq!(mem.board(Utc::now()).unwrap().questions[0].id, "ask1");

    // Basil's shard lists it.
    let s2 = open_shard(&mem, "s2", "Basil");
    let brief = s2.questions_brief(Some("Basil")).unwrap();
    assert!(
        brief.contains("ask1") && brief.contains("Mike") && brief.contains("only in passing"),
        "{brief}"
    );
    // An answer must be about the question's fact, and goes through a shard.
    let e = s2
        .supersede(
            &addr("kiln/east/temp"),
            "910",
            &answering("Basil saw it", "ask1"),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("asks about lathe/two/spindle"),
        "{e}"
    );
    let e = mem
        .supersede(&addr("lathe/two/spindle"), "x", &{
            let mut p = core("y");
            p.answers = Some("ask1".into());
            p
        })
        .unwrap_err();
    assert!(e.to_string().contains("goes through a shard"), "{e}");

    s2.supersede(
        &addr("lathe/two/spindle"),
        "replaced 2026-11",
        &answering("Basil: yes, he helped Mike swap it on the 3rd", "ask1"),
    )
    .unwrap();
    let r = mem.dinit("s2", false).unwrap();
    assert_eq!(r.proposal.as_deref(), Some("p2"));
    let p2 = mem.proposal("p2").unwrap();
    assert_eq!(p2.changes[0].answers.as_deref(), Some("ask1"));
    // It is a proposal like any other: it wakes the core; nothing on main yet.
    assert_eq!(val(&mem, "lathe/two/spindle").as_deref(), Some("original"));
    let fired = mem.watch_pass(Utc::now()).unwrap().fired;
    assert!(fired
        .iter()
        .any(|e| e.kind == EventKind::Proposal && e.key == "proposal:p2"));
    // The all-held p1 doesn't wake the core.
    assert!(!fired.iter().any(|e| e.key == "proposal:p1"));

    mem.proposal_decide(
        "p2",
        Some(1),
        &Verdict::Accept(None),
        &core("Basil confirmed it"),
    )
    .unwrap();
    assert_eq!(
        val(&mem, "lathe/two/spindle").as_deref(),
        Some("replaced 2026-11")
    );
    let q = mem.question("ask1").unwrap();
    assert_eq!(q.status, QuestionStatus::Closed);
    assert_eq!(q.closed.as_ref().unwrap().by_change, "p2#1");
    let p1 = mem.proposal("p1").unwrap();
    assert_eq!(p1.changes[0].status, ChangeStatus::Answered);
    assert_eq!(p1.changes[0].answered_by.as_deref(), Some("p2#1"));
    assert_eq!(p1.log.last().unwrap().action, "answered");
    assert!(!p1.is_open());
    assert_eq!(
        mem.proposal("p2").unwrap().log[0].question.as_deref(),
        Some("ask1")
    );
    assert!(mem.board(Utc::now()).unwrap().questions.is_empty());
    // A closed question can't be answered again.
    let s3 = open_shard(&mem, "s3", "Dee");
    assert!(s3
        .supersede(&addr("lathe/two/spindle"), "z", &answering("w", "ask1"))
        .is_err());
}

#[test]
fn dont_know_and_a_rejected_answer_leave_the_question_open() {
    let (_t, mem) = fresh();
    held_spindle(&mem);
    let s2 = open_shard(&mem, "s2", "Basil");
    s2.question_dont_know("ask1", &shardp("Basil wasn't in that week"))
        .unwrap();
    let q = mem.question("ask1").unwrap();
    assert!(q.is_open());
    assert_eq!(q.log[0].kind, "dont-know");
    assert_eq!(q.log[0].human.as_deref(), Some("Basil"));
    assert_eq!(q.log[0].by, "s2");
    // A reason is required.
    assert!(s2.question_dont_know("ask1", &shardp("")).is_err());
    // Still listed for the next person, with who didn't know.
    let s3 = open_shard(&mem, "s3", "Cleo");
    let brief = s3.questions_brief(Some("Cleo")).unwrap();
    assert!(brief.contains("didn't know: Basil"), "{brief}");

    // A rejected answer is logged; the question stays open.
    s3.supersede(
        &addr("lathe/two/spindle"),
        "never replaced",
        &answering("Cleo thinks not", "ask1"),
    )
    .unwrap();
    mem.dinit("s3", false).unwrap();
    mem.proposal_decide("p2", Some(1), &Verdict::Reject, &core("Cleo was guessing"))
        .unwrap();
    let q = mem.question("ask1").unwrap();
    assert!(q.is_open());
    assert_eq!(q.log.last().unwrap().kind, "answer-rejected");
    assert_eq!(
        mem.proposal("p1").unwrap().changes[0].status,
        ChangeStatus::Held
    );

    // A human deciding the held change directly closes the question too.
    mem.proposal_decide(
        "p1",
        Some(1),
        &Verdict::Accept(None),
        &Provenance::new("core", Some("Mike".into()), "Mike confirmed in person"),
    )
    .unwrap();
    let q = mem.question("ask1").unwrap();
    assert_eq!(q.status, QuestionStatus::Closed);
    assert_eq!(q.closed.unwrap().by_change, "p1#1");
}

#[test]
fn a_proposal_keeps_the_shards_drafts_in_order() {
    let (_t, mem) = fresh();
    let s1 = open_shard(&mem, "s1", "Ada");
    let a = addr("kiln/east/temp");
    s1.supersede(&a, "910", &shardp("first read, probe still warming"))
        .unwrap();
    s1.supersede(&a, "925", &shardp("second read")).unwrap();
    s1.supersede(&a, "930", &shardp("settled at 930")).unwrap();
    mem.dinit("s1", false).unwrap();
    let c = mem.proposal("p1").unwrap().changes[0].clone();
    assert_eq!(c.value.as_deref(), Some("930"));
    let drafts: Vec<(Option<&str>, &str)> = c
        .drafts
        .iter()
        .map(|d| (d.value.as_deref(), d.why.as_str()))
        .collect();
    assert_eq!(
        drafts,
        vec![
            (Some("910"), "first read, probe still warming"),
            (Some("925"), "second read"),
        ]
    );
    // Drafts never reach main's history; only the core's decision does.
    mem.proposal_decide("p1", None, &Verdict::Accept(None), &core("ok"))
        .unwrap();
    let values: Vec<String> = mem
        .history(&a)
        .unwrap()
        .into_iter()
        .map(|e| e.value)
        .collect();
    assert_eq!(values, vec!["930", "900"]);
    let d = mem.drafts_for("kiln/east/temp").unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].1.drafts.len(), 2);
}
