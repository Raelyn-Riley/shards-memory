// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 07: the known-quirks board. All names are fake.

use shards_core::{Context, Error, Memory, NewQuirk, Provenance, QuirkState};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn on_main() -> Provenance {
    Provenance::new("core", None, "")
}

/// In a shard worktree the author defaults to the shard id.
fn in_shard() -> Provenance {
    Provenance::new("", None, "")
}

fn at(t: &str) -> Provenance {
    let mut p = on_main();
    p.at = Some(t.to_string());
    p
}

fn fresh() -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &on_main(), env()).unwrap();
    (tmp, Memory::open_with_env(&root, env()).unwrap())
}

fn shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    let s = Memory::open_with_env(&meta.worktree, env()).unwrap();
    assert_eq!(s.context(), &Context::Shard(id.to_string()));
    s
}

fn new(tool: &str, quirk: &str, by: &str) -> NewQuirk {
    NewQuirk {
        tool: tool.into(),
        quirk: quirk.into(),
        by: by.into(),
        details: None,
    }
}

fn open_ids(m: &Memory) -> Vec<String> {
    m.quirks()
        .unwrap()
        .into_iter()
        .filter(|q| q.state == QuirkState::Open)
        .map(|q| q.id)
        .collect()
}

#[test]
fn add_list_update_fix_archive() {
    let (_t, mem) = fresh();
    let mut n = new("bandsaw-2", "file handle is loose", "Bob");
    n.details = Some("hold it with your left hand\nuntil the new pin arrives".into());
    let q = mem.quirk_add(&n, &at("2026-11-01T09:00:00Z")).unwrap();
    assert_eq!(q.id, "q1");
    assert_eq!(q.reported, "2026-11-01T09:00:00Z");
    assert_eq!(q.status, "open");
    let q2 = mem
        .quirk_add(&new("kiln-east", "door sticks when hot", "Ada"), &on_main())
        .unwrap();
    assert_eq!(q2.id, "q2");
    // The date is automatic: close to now.
    let t = chrono::DateTime::parse_from_rfc3339(&q2.reported).unwrap();
    assert!((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < 60);

    let got = mem.quirk("1").unwrap();
    assert_eq!(got.tool, "bandsaw-2");
    assert_eq!(got.reported_by, "Bob");
    assert_eq!(
        got.details.as_deref(),
        Some("hold it with your left hand\nuntil the new pin arrives")
    );
    assert_eq!(open_ids(&mem), ["q1", "q2"]);

    let u = mem
        .quirk_update(
            "q1",
            "worse in the mornings",
            "Jim",
            &at("2026-11-02T09:00:00Z"),
        )
        .unwrap();
    assert_eq!(u.status, "worse in the mornings");
    assert_eq!(u.status_by, "Jim");
    assert_eq!(u.state, QuirkState::Open);

    let f = mem
        .quirk_fix("q1", "Ada", &at("2026-11-03T09:00:00Z"))
        .unwrap();
    assert_eq!(f.status, "fixed by Ada");
    assert_eq!(f.state, QuirkState::Fixed);
    assert_eq!(open_ids(&mem), ["q2"]);

    // Not quiet long enough yet.
    assert!(mem
        .quirk_archive(30.0, &at("2026-11-20T09:00:00Z"))
        .unwrap()
        .is_empty());
    let done = mem
        .quirk_archive(30.0, &at("2026-12-04T09:00:00Z"))
        .unwrap();
    assert_eq!(done.len(), 1);
    let a = mem.quirk("q1").unwrap();
    assert_eq!(a.state, QuirkState::Archived);
    assert_eq!(a.status, "fixed by Ada");
    // Archived quirks stay on record.
    assert_eq!(mem.quirks().unwrap().len(), 2);

    assert!(matches!(mem.quirk("q9"), Err(Error::UnknownQuirk(_))));
    assert!(matches!(
        mem.quirk_fix("q9", "Ada", &on_main()),
        Err(Error::UnknownQuirk(_))
    ));
    assert!(mem.quirk_add(&new("", "x", "Bob"), &on_main()).is_err());
    assert!(mem
        .quirk_add(&new("lathe", "two\nlines", "Bob"), &on_main())
        .is_err());
    assert!(mem.quirk_update("q2", "x", "", &on_main()).is_err());
}

#[test]
fn history_keeps_every_status() {
    let (_t, mem) = fresh();
    mem.quirk_add(
        &new("lathe-1", "chuck key wobbles", "Bob"),
        &at("2026-11-01T09:00:00Z"),
    )
    .unwrap();
    mem.quirk_update(
        "q1",
        "only with the big chuck",
        "Jim",
        &at("2026-11-02T09:00:00Z"),
    )
    .unwrap();
    mem.quirk_fix("q1", "Ada", &at("2026-11-03T09:00:00Z"))
        .unwrap();
    let h = mem.quirk_history("q1").unwrap();
    let rows: Vec<(&str, &str, &str, &str)> = h
        .iter()
        .map(|v| {
            (
                v.op.as_str(),
                v.quirk.status.as_str(),
                v.quirk.status_by.as_str(),
                v.at(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("quirk-fix", "fixed by Ada", "Ada", "2026-11-03T09:00:00Z"),
            (
                "quirk-update",
                "only with the big chuck",
                "Jim",
                "2026-11-02T09:00:00Z"
            ),
            ("quirk-add", "open", "Bob", "2026-11-01T09:00:00Z"),
        ]
    );
    assert!(h.iter().all(|v| v.author == "core"));
}

#[test]
fn visible_from_another_worktree_at_once_without_dinit() {
    let (_t, mem) = fresh();
    let a = shard(&mem, "s1", "Bob");
    let b = shard(&mem, "s2", "Jim");
    let b_head = b.head().unwrap();

    let q = a
        .quirk_add(
            &new("bandsaw-2", "file handle is loose", "Bob"),
            &in_shard(),
        )
        .unwrap();
    // Shard B sees it straight away; its branch has not moved.
    let seen = b.quirks().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, q.id);
    assert_eq!(seen[0].quirk, "file handle is loose");
    assert_eq!(b.head().unwrap(), b_head);
    // It went to main, not to shard A's branch, and was authored by s1.
    assert_eq!(mem.quirks().unwrap().len(), 1);
    assert!(a.read_file_at("HEAD", "quirks/q1.md").unwrap().is_none());
    assert_eq!(b.quirk_history("q1").unwrap()[0].author, "s1");
    // B fixes it; A sees that at once too.
    b.quirk_fix("q1", "Jim", &in_shard()).unwrap();
    assert_eq!(a.quirk("q1").unwrap().status, "fixed by Jim");
    // A shard may not pose as another author.
    assert!(matches!(
        a.quirk_add(&new("x", "y", "Bob"), &Provenance::new("s2", None, "")),
        Err(Error::NotYourShard { .. })
    ));

    // A dinit neither sees nor disturbs quirks.
    let r = mem.dinit("s1", false).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert_eq!(mem.quirk("q1").unwrap().status, "fixed by Jim");
}

#[test]
fn concurrent_adds_lose_nothing() {
    let (_t, mem) = fresh();
    let shards: Vec<Memory> = (1..=4)
        .map(|i| shard(&mem, &format!("s{i}"), &format!("Tester{i}")))
        .collect();
    let mut handles = Vec::new();
    for (i, s) in shards.into_iter().enumerate() {
        handles.push(std::thread::spawn(move || {
            for j in 0..5 {
                s.quirk_add(
                    &new("drill-press", &format!("quirk {i}-{j}"), "Bob"),
                    &in_shard(),
                )
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let all = mem.quirks().unwrap();
    assert_eq!(all.len(), 20);
    let mut ids: Vec<String> = all.iter().map(|q| q.id.clone()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 20, "ids must be unique");
    let mut texts: Vec<String> = all.iter().map(|q| q.quirk.clone()).collect();
    texts.sort();
    texts.dedup();
    assert_eq!(texts.len(), 20, "every write landed");
}

#[test]
fn duplicates_are_allowed() {
    let (_t, mem) = fresh();
    mem.quirk_add(&new("kiln-east", "door sticks", "Ada"), &on_main())
        .unwrap();
    mem.quirk_add(&new("kiln-east", "door sticks", "Bob"), &on_main())
        .unwrap();
    assert_eq!(open_ids(&mem), ["q1", "q2"]);
}

#[test]
fn open_quirks_never_archive() {
    let (_t, mem) = fresh();
    mem.quirk_add(
        &new("router", "bit slips", "Bob"),
        &at("2020-01-01T00:00:00Z"),
    )
    .unwrap();
    mem.quirk_update("q1", "still slips", "Jim", &at("2020-02-01T00:00:00Z"))
        .unwrap();
    for days in [0.0, 1.0, 30.0] {
        assert!(mem
            .quirk_archive(days, &at("2026-11-07T00:00:00Z"))
            .unwrap()
            .is_empty());
    }
    assert_eq!(mem.quirk("q1").unwrap().state, QuirkState::Open);
    // A fixed-then-reopened quirk is open again and never archives either.
    mem.quirk_fix("q1", "Ada", &at("2020-03-01T00:00:00Z"))
        .unwrap();
    mem.quirk_update("q1", "slipping again", "Bob", &at("2020-04-01T00:00:00Z"))
        .unwrap();
    assert!(mem
        .quirk_archive(0.0, &at("2026-11-07T00:00:00Z"))
        .unwrap()
        .is_empty());
    assert!(mem.quirk_archive(-1.0, &on_main()).is_err());
}

#[test]
fn a_mention_resets_the_clock() {
    let (_t, mem) = fresh();
    mem.quirk_add(
        &new("planer", "snipes the ends", "Bob"),
        &at("2026-10-01T00:00:00Z"),
    )
    .unwrap();
    mem.quirk_fix("q1", "Ada", &at("2026-10-01T00:00:00Z"))
        .unwrap();
    // It came up again 20 days after the fix.
    mem.quirk_mention("q1", "Jim", &at("2026-10-21T00:00:00Z"))
        .unwrap();
    // 31 days after the fix, but only 11 after the mention: stays fixed.
    assert!(mem
        .quirk_archive(30.0, &at("2026-11-01T00:00:00Z"))
        .unwrap()
        .is_empty());
    assert_eq!(mem.quirk("q1").unwrap().state, QuirkState::Fixed);
    // 30 days after the mention: archived.
    assert_eq!(
        mem.quirk_archive(30.0, &at("2026-11-20T00:00:00Z"))
            .unwrap()
            .len(),
        1
    );
    // A mention of an archived quirk brings it back to fixed, with a fresh clock.
    let q = mem
        .quirk_mention("q1", "", &at("2026-11-25T00:00:00Z"))
        .unwrap();
    assert_eq!(q.state, QuirkState::Fixed);
    assert_eq!(q.archived_at, None);
    assert!(mem
        .quirk_archive(30.0, &at("2026-12-01T00:00:00Z"))
        .unwrap()
        .is_empty());
    // History shows the archive and both mentions.
    let ops: Vec<String> = mem
        .quirk_history("q1")
        .unwrap()
        .into_iter()
        .map(|v| v.op)
        .collect();
    assert_eq!(
        ops,
        [
            "quirk-mention",
            "quirk-archive",
            "quirk-mention",
            "quirk-fix",
            "quirk-add"
        ]
    );
}

#[test]
fn archive_days_come_from_config() {
    let (_t, mem) = fresh();
    std::fs::write(
        mem.root().join("shards.toml"),
        "[quirks]\narchive_after_days = 7\n",
    )
    .unwrap();
    let mem = Memory::open_with_env(mem.root(), env()).unwrap();
    assert_eq!(mem.config().quirks.archive_after_days, 7.0);
}
