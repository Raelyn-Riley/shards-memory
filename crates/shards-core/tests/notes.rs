// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 09: notes with owners. `notes/<person>/` is a person's own;
//! notes at the top of `notes/` are shared. All names are fake.

use shards_core::{Address, Context, Memory, Owner, Provenance, Rank};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn at(author: &str, t: &str) -> Provenance {
    let mut p = Provenance::new(author, None, "note");
    p.at = Some(t.to_string());
    p
}

fn fresh() -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &Provenance::new("core", None, ""), env()).unwrap();
    (tmp, Memory::open_with_env(&root, env()).unwrap())
}

fn shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    let s = Memory::open_with_env(&meta.worktree, env()).unwrap();
    assert_eq!(s.context(), &Context::Shard(id.to_string()));
    s
}

#[test]
fn put_for_a_person_and_shared_notes_still_work() {
    let (_t, mem) = fresh();
    let p = at("core", "2026-11-01T09:00:00Z");
    let (path, _) = mem
        .put_note_as(Some("ada"), "notes/kiln.md", "kiln east runs hot\n", &p)
        .unwrap();
    assert_eq!(path, "notes/ada/kiln.md");
    let (path, _) = mem
        .put_note_as(None, "notes/runbook.md", "check the kiln\n", &p)
        .unwrap();
    assert_eq!(path, "notes/runbook.md");
    // The old entry points still write shared notes.
    mem.put_file("notes/shop.md", "kiln key on the hook\n", &p)
        .unwrap();
    mem.put_note("notes/door.md", "door sticks\n", &p).unwrap();
    assert_eq!(
        mem.read_file_at("HEAD", "notes/ada/kiln.md")
            .unwrap()
            .as_deref(),
        Some("kiln east runs hot\n")
    );
    // A person's note can't land outside notes/.
    assert!(mem
        .put_note_as(Some("ada"), "people/ada.md", "x\n", &p)
        .is_err());
    assert!(mem
        .put_note_as(Some("ada"), "facts/devices/kiln/ip.md", "x\n", &p)
        .is_err());
    assert!(mem
        .put_note_as(Some("../soul"), "core.md", "x\n", &p)
        .is_err());
}

#[test]
fn find_lists_own_then_shared_then_everyone_else_with_owner_and_date() {
    let (_t, mem) = fresh();
    let put = |person: Option<&str>, path: &str, text: &str, t: &str| {
        mem.put_note_as(person, path, text, &at("core", t)).unwrap();
    };
    put(
        Some("basil"),
        "kiln.md",
        "Kiln east: use the left vent\n",
        "2026-11-05T10:00:00Z",
    );
    put(
        None,
        "notes/runbook.md",
        "the kiln key is on the hook\n",
        "2026-11-02T10:00:00Z",
    );
    put(
        Some("ada"),
        "old.md",
        "kiln shelf cracked\n",
        "2026-10-01T10:00:00Z",
    );
    put(
        Some("ada"),
        "new.md",
        "KILN timer reset\n",
        "2026-11-06T10:00:00Z",
    );
    put(
        None,
        "notes/shop.md",
        "nothing about it\n",
        "2026-11-06T11:00:00Z",
    );
    put(
        Some("jun"),
        "lathe.md",
        "lathe chuck is stiff\n",
        "2026-11-06T12:00:00Z",
    );
    // A fact that mentions the word is never a note.
    mem.set(
        &Address::parse("devices/kiln-east/ip").unwrap(),
        "10.0.4.40",
        &at("core", "2026-11-06T13:00:00Z"),
    )
    .unwrap();

    let hits = mem.find_notes("kiln", "ada").unwrap();
    let got: Vec<(&str, Rank, &str)> = hits
        .iter()
        .map(|h| (h.path.as_str(), h.rank, h.day()))
        .collect();
    assert_eq!(
        got,
        [
            ("notes/ada/new.md", Rank::Own, "2026-11-06"),
            ("notes/ada/old.md", Rank::Own, "2026-10-01"),
            ("notes/runbook.md", Rank::Shared, "2026-11-02"),
            ("notes/basil/kiln.md", Rank::Other, "2026-11-05"),
        ]
    );
    assert_eq!(hits[0].owner, Owner::Person("ada".into()));
    assert_eq!(hits[2].owner, Owner::Shared);
    assert_eq!(hits[3].owner, Owner::Person("basil".into()));
    assert_eq!(hits[0].lines, ["KILN timer reset"]);
    assert!(hits.iter().all(|h| h.path.starts_with("notes/")));

    // For Basil, his own comes first.
    let hits = mem.find_notes("kiln", "basil").unwrap();
    assert_eq!(hits[0].path, "notes/basil/kiln.md");
    assert_eq!(hits[0].rank, Rank::Own);
    assert_eq!(hits[1].rank, Rank::Shared);
    assert!(hits[2..].iter().all(|h| h.rank == Rank::Other));

    // Someone with no notes of their own gets shared, then everyone's.
    let hits = mem.find_notes("lathe", "tess").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rank, Rank::Other);
    assert!(mem.find_notes("bandsaw", "ada").unwrap().is_empty());
    assert!(mem.find_notes(" ", "ada").is_err());
}

#[test]
fn a_persons_note_rides_the_shard_branch_and_merges_at_dinit() {
    let (_t, mem) = fresh();
    let s1 = shard(&mem, "s1", "Ada");
    let (path, _) = s1
        .put_note_as(
            Some("ada"),
            "kiln.md",
            "vent left\n",
            &at("", "2026-11-06T10:00:00Z"),
        )
        .unwrap();
    assert_eq!(path, "notes/ada/kiln.md");
    // On the branch, not on main yet.
    assert_eq!(s1.find_notes("vent", "ada").unwrap().len(), 1);
    assert!(mem.find_notes("vent", "ada").unwrap().is_empty());
    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.merged, ["notes/ada/kiln.md"]);
    assert!(r.conflicts.is_empty() && r.proposals.is_empty());
    assert!(r.closed);
    let hits = mem.find_notes("vent", "ada").unwrap();
    assert_eq!(hits[0].path, "notes/ada/kiln.md");
    assert_eq!(hits[0].day(), "2026-11-06");
}

// Brief 10: a person matches in any case; a shard reads main's notes too.

fn put(mem: &Memory, person: Option<&str>, path: &str, text: &str, t: &str) -> String {
    // In a shard the author is the shard id (left empty here).
    let who = if mem.context() == &Context::Main {
        "core"
    } else {
        ""
    };
    mem.put_note_as(person, path, text, &at(who, t)).unwrap().0
}

#[test]
fn a_person_is_the_same_in_any_case() {
    let (_t, mem) = fresh();
    // No folder yet: a new one is lowercase, whatever case is given.
    let t = "2026-11-01T09:00:00Z";
    assert_eq!(
        put(&mem, Some("Dave"), "kiln.md", "kiln a\n", t),
        "notes/dave/kiln.md"
    );
    assert_eq!(
        put(&mem, Some("DAVE"), "vent.md", "kiln b\n", t),
        "notes/dave/vent.md"
    );
    assert_eq!(
        put(&mem, Some("dAvE"), "notes/Dave/shelf.md", "kiln c\n", t),
        "notes/dave/shelf.md"
    );
    // An existing folder keeps its case.
    mem.put_file("notes/MINA/old.md", "kiln d\n", &at("core", t))
        .unwrap();
    assert_eq!(
        put(&mem, Some("mina"), "kiln.md", "kiln e\n", t),
        "notes/MINA/kiln.md"
    );
    assert_eq!(
        put(&mem, Some("Mina"), "notes/mina/x.md", "kiln f\n", t),
        "notes/MINA/x.md"
    );
    assert_eq!(mem.note_folder("MiNa").unwrap(), "MINA");
    // A folder that only starts with the name is someone else.
    mem.put_file("notes/minato/a.md", "kiln g\n", &at("core", t))
        .unwrap();
    assert_eq!(mem.note_folder("mina").unwrap(), "MINA");

    // find: --for in any case ranks the folder in any case as own.
    for who in ["mina", "Mina", "MINA"] {
        let hits = mem.find_notes("kiln", who).unwrap();
        let own: Vec<&str> = hits
            .iter()
            .filter(|h| h.rank == Rank::Own)
            .map(|h| h.path.as_str())
            .collect();
        assert_eq!(own.len(), 3, "{who}: {own:?}");
        assert!(own.iter().all(|p| p.starts_with("notes/MINA/")), "{own:?}");
        assert_eq!(hits[0].owner, Owner::Person("MINA".into()));
    }
    let hits = mem.find_notes("kiln", "DAVE").unwrap();
    assert_eq!(hits.iter().filter(|h| h.rank == Rank::Own).count(), 3);

    // Two folders that differ only by case (written by hand before brief 10):
    // both are the person's; a put goes to the one spelled as given, else the
    // first by name.
    mem.put_file("notes/Dave/hand.md", "kiln h\n", &at("core", t))
        .unwrap();
    assert_eq!(mem.note_folder("Dave").unwrap(), "Dave");
    assert_eq!(mem.note_folder("dave").unwrap(), "dave");
    assert_eq!(mem.note_folder("DAVE").unwrap(), "Dave");
    let hits = mem.find_notes("kiln", "dave").unwrap();
    assert_eq!(hits.iter().filter(|h| h.rank == Rank::Own).count(), 4);
}

#[test]
fn a_shard_finds_notes_merged_to_main_after_it_opened() {
    let (_t, mem) = fresh();
    put(
        &mem,
        None,
        "notes/runbook.md",
        "kiln key on the hook\n",
        "2026-11-01T09:00:00Z",
    );
    put(
        &mem,
        Some("ada"),
        "gone.md",
        "kiln old shelf\n",
        "2026-11-01T09:00:00Z",
    );
    let s1 = shard(&mem, "s1", "Ada");
    let s2 = shard(&mem, "s2", "Basil");

    // After s1 opened: s2 writes and merges; main is also written directly.
    put(
        &s2,
        Some("Basil"),
        "kiln.md",
        "kiln east: left vent\n",
        "2026-11-05T10:00:00Z",
    );
    put(
        &s2,
        None,
        "notes/runbook.md",
        "kiln key moved to the drawer\n",
        "2026-11-05T10:00:00Z",
    );
    assert!(mem.dinit("s2", false).unwrap().conflicts.is_empty());
    put(
        &mem,
        Some("ada"),
        "timer.md",
        "kiln timer reset\n",
        "2026-11-04T10:00:00Z",
    );
    put(
        &mem,
        None,
        "notes/shop.md",
        "kiln gloves\n",
        "2026-11-03T10:00:00Z",
    );

    // s1 edits the shared runbook (also changed on main) and adds its own.
    put(
        &s1,
        None,
        "notes/runbook.md",
        "kiln key: ask Ada\n",
        "2026-11-06T10:00:00Z",
    );
    put(
        &s1,
        Some("ADA"),
        "shelf.md",
        "kiln shelf cracked\n",
        "2026-11-06T11:00:00Z",
    );
    // And deletes one of hers that was there when it opened.
    let g = std::process::Command::new("git")
        .current_dir(s1.root())
        .envs(env())
        .args(["rm", "-q", "notes/ada/gone.md"])
        .status()
        .unwrap();
    assert!(g.success());
    let g = std::process::Command::new("git")
        .current_dir(s1.root())
        .envs(env())
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-qm",
            "rm",
        ])
        .status()
        .unwrap();
    assert!(g.success());

    let hits = s1.find_notes("kiln", "Ada").unwrap();
    let got: Vec<(&str, Rank, &str)> = hits
        .iter()
        .map(|h| (h.path.as_str(), h.rank, h.day()))
        .collect();
    assert_eq!(
        got,
        [
            ("notes/ada/shelf.md", Rank::Own, "2026-11-06"),
            ("notes/ada/timer.md", Rank::Own, "2026-11-04"),
            ("notes/runbook.md", Rank::Shared, "2026-11-06"),
            ("notes/shop.md", Rank::Shared, "2026-11-03"),
            ("notes/basil/kiln.md", Rank::Other, "2026-11-05"),
        ]
    );
    // The runbook counts once, with the shard's text.
    assert_eq!(hits[2].lines, ["kiln key: ask Ada"]);

    // Main alone is unchanged: it never sees s1's branch.
    let on_main: Vec<String> = mem
        .find_notes("kiln", "ada")
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect();
    assert!(!on_main.contains(&"notes/ada/shelf.md".to_string()));
    assert!(on_main.contains(&"notes/ada/gone.md".to_string()));
}

#[test]
fn a_shard_puts_into_a_folder_that_reached_main_after_it_opened() {
    let (_t, mem) = fresh();
    let s1 = shard(&mem, "s1", "Ada");
    mem.put_file(
        "notes/JUN/lathe.md",
        "chuck stiff\n",
        &at("core", "2026-11-02T09:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        put(
            &s1,
            Some("jun"),
            "kiln.md",
            "kiln note\n",
            "2026-11-03T09:00:00Z"
        ),
        "notes/JUN/kiln.md"
    );
    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.merged, ["notes/JUN/kiln.md"]);
    assert!(r.conflicts.is_empty());
}
