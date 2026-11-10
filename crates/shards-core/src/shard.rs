// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Shards (a branch + worktree per session), dinit (the locked merge into
//! main), conflict records, and soul approval.
//!
//! # How dinit merges
//!
//! Dinit makes one real two-parent merge commit on main (`main` + the shard
//! tip) and fast-forwards main to it with `git merge --ff-only`. The merge
//! tree is built by shards itself in a temporary index, path by path, from
//! the rules below; git's own content merge is never used. Because the shard
//! tip becomes an ancestor of main:
//!
//! - every shard commit (each version with its author/time/why) is reachable
//!   from main, and `mem history` on main shows them;
//! - the next dinit's base is simply `git merge-base main shard/<id>`, so a
//!   shard can keep working and dinit again after a partial dinit.
//!
//! Per path changed on the shard since the base:
//!
//! | path | rule |
//! | --- | --- |
//! | `notes/` | same as main → nothing to do; changed on main too → conflict; else applied |
//! | `facts/`, `people/` | same as main → nothing to do; changed on main too → conflict; else **proposed**: one record, `proposals/p<n>.json`, for the core to review (see [`crate::proposal`]) |
//! | `soul/` | never applied; a proposal for a human admin (`mem soul approve`) |
//! | anything else (`shards.toml`, `conflicts/`, README) | skipped and reported |
//!
//! A conflict leaves main's version in place and writes a record to
//! `conflicts/<shard>/<address>.md` (for non-fact files,
//! `conflicts/<shard>/@/<path>`) in the same merge commit.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::error::{Error, Result};
use crate::fact::{parse_front, render_front, Fact as FactFile};
use crate::git::Opts;
use crate::incident::{Incident, IncidentSide, Settlement};
use crate::memory::{fact_file, now, FileWrite, Memory, Provenance};

/// What `.shards/shards/<id>.json` records about an open shard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardMeta {
    pub id: String,
    pub human: Option<String>,
    pub branch: String,
    /// main's commit when the shard was opened.
    pub base: String,
    pub opened: String,
    pub worktree: PathBuf,
    /// soul/ paths proposed at earlier dinits, awaiting an admin.
    #[serde(default)]
    pub soul_pending: Vec<String>,
    /// The shard commit the pending soul files are taken from.
    #[serde(default)]
    pub soul_commit: Option<String>,
    /// The shard's agent session, pinned by `mem shard start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::shardsession::ShardSession>,
}

/// A one-line gist of what a shard is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardStatus {
    pub gist: String,
    pub at: String,
}

/// One row of `mem shard list`.
#[derive(Debug, Clone)]
pub struct ShardInfo {
    pub meta: ShardMeta,
    pub last_commit: String,
    pub status: Option<ShardStatus>,
}

/// One side of a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub value: String,
    pub author: String,
    pub human: Option<String>,
    pub updated: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Fact address, or the repo path for non-fact files.
    pub target: String,
    pub path: String,
    pub record: String,
    pub main: Side,
    pub shard: Side,
}

#[derive(Debug, Clone, Default)]
pub struct DinitReport {
    pub shard: String,
    /// Paths applied to main (notes).
    pub merged: Vec<String>,
    /// The proposal holding the shard's fact and people changes, if any.
    pub proposal: Option<String>,
    /// The paths in that proposal.
    pub proposed: Vec<String>,
    /// Paths changed on the shard that already match main.
    pub unchanged: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// soul/ paths awaiting an admin (including earlier ones).
    pub proposals: Vec<String>,
    /// Paths dinit never merges (config, conflict records, README, ...).
    pub skipped: Vec<String>,
    /// The merge commit, if one was made.
    pub commit: Option<String>,
    /// `--all-or-nothing` and conflicts: nothing was written.
    pub aborted: bool,
    /// Clean dinit: worktree removed, shard closed.
    pub closed: bool,
    pub branch_deleted: bool,
    /// Why the shard stayed open despite a clean dinit, if it did.
    pub kept_open: Option<String>,
    /// A clean dinit stopped the shard's running session.
    pub session_stopped: bool,
}

/// What settling a conflict did.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub commit: String,
    /// The incident written for it, e.g. `i3`.
    pub incident: String,
}

/// How to settle a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Take {
    Shard,
    Main,
    Value(String),
}

impl Take {
    pub fn parse(s: &str) -> Take {
        match s {
            "shard" => Take::Shard,
            "main" => Take::Main,
            v => Take::Value(v.to_string()),
        }
    }
    fn label(&self) -> &str {
        match self {
            Take::Shard => "shard",
            Take::Main => "main",
            Take::Value(_) => "value",
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
}

/// `facts/a/b.md` → `a/b`; other paths are their own target.
pub(crate) fn target_of(path: &str) -> String {
    Address::from_home(path)
        .map(|a| a.to_string())
        .unwrap_or_else(|| path.to_string())
}

pub(crate) fn record_path(shard: &str, path: &str) -> String {
    match Address::from_home(path) {
        Some(a) => format!("conflicts/{shard}/{a}.md"),
        None => format!("conflicts/{shard}/@/{path}"),
    }
}

impl Memory {
    fn shards_dir(&self) -> PathBuf {
        self.state_dir().join("shards")
    }

    fn status_path(&self, id: &str) -> PathBuf {
        self.state_dir().join("status").join(format!("{id}.json"))
    }

    pub fn shard_meta(&self, id: &str) -> Result<ShardMeta> {
        let text = fs::read_to_string(self.shards_dir().join(format!("{id}.json")))
            .map_err(|_| Error::UnknownShard(id.to_string()))?;
        serde_json::from_str(&text).map_err(|e| Error::Malformed {
            path: format!(".shards/shards/{id}.json"),
            reason: e.to_string(),
        })
    }

    pub(crate) fn save_meta(&self, meta: &ShardMeta) -> Result<()> {
        fs::create_dir_all(self.shards_dir())?;
        let tmp = self.shards_dir().join(format!(".{}.json.tmp", meta.id));
        fs::write(
            &tmp,
            serde_json::to_string_pretty(meta).expect("serializable"),
        )?;
        fs::rename(tmp, self.shards_dir().join(format!("{}.json", meta.id)))?;
        Ok(())
    }

    /// Keep `.shards/` out of git even in memories made before it was in
    /// `.gitignore`.
    fn ensure_state_excluded(&self) -> Result<()> {
        let common = self
            .git
            .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let exclude = PathBuf::from(common.trim()).join("info").join("exclude");
        let cur = fs::read_to_string(&exclude).unwrap_or_default();
        if !cur.lines().any(|l| l.trim() == "/.shards/") {
            fs::create_dir_all(exclude.parent().expect("has parent"))?;
            fs::write(
                &exclude,
                format!(
                    "{cur}{}/.shards/\n",
                    if cur.is_empty() || cur.ends_with('\n') {
                        ""
                    } else {
                        "\n"
                    }
                ),
            )?;
        }
        Ok(())
    }

    /// Open a shard: branch `shard/<id>` from current main, in its own worktree.
    pub fn shard_open(&self, id: &str, human: Option<&str>) -> Result<ShardMeta> {
        if !valid_id(id) {
            return Err(Error::BadShardId(id.to_string()));
        }
        let main = self.main()?;
        let branch = format!("shard/{id}");
        if main.shard_meta(id).is_ok()
            || main.git.probe(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])?
        {
            return Err(Error::ShardExists(id.to_string()));
        }
        main.ensure_state_excluded()?;
        let wt_dir = PathBuf::from(&main.config.shards.worktree_dir);
        let worktree = if wt_dir.is_absolute() {
            wt_dir.join(id)
        } else {
            main.main_root.join(wt_dir).join(id)
        };
        let _git = main.git_mutex()?;
        let base = main.git.run(&["rev-parse", "main"])?.trim().to_string();
        let wt = worktree.to_string_lossy().to_string();
        main.git
            .run(&["worktree", "add", "--quiet", "-b", &branch, &wt, &base])?;
        let meta = ShardMeta {
            id: id.to_string(),
            human: human.map(String::from).filter(|h| !h.is_empty()),
            branch,
            base,
            opened: now(),
            worktree,
            soul_pending: Vec::new(),
            soul_commit: None,
            session: None,
        };
        main.save_meta(&meta)?;
        Ok(meta)
    }

    /// All open shards, sorted by id.
    pub fn shard_list(&self) -> Result<Vec<ShardInfo>> {
        let mut out = Vec::new();
        let dir = self.shards_dir();
        if !dir.is_dir() {
            return Ok(out);
        }
        let mut ids: Vec<String> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".json")
                    .filter(|n| !n.starts_with('.'))
                    .map(String::from)
            })
            .collect();
        ids.sort();
        for id in ids {
            let meta = self.shard_meta(&id)?;
            let last_commit = self
                .git
                .run(&["log", "-1", "--format=%aI", &meta.branch])
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let status = fs::read_to_string(self.status_path(&id))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            out.push(ShardInfo {
                meta,
                last_commit,
                status,
            });
        }
        Ok(out)
    }

    /// Set a shard's one-line status. Stored outside git, so it can never
    /// cause a merge conflict.
    pub fn shard_set_status(&self, id: &str, gist: &str) -> Result<ShardStatus> {
        self.shard_meta(id)?;
        if gist.contains('\n') || gist.contains('\r') {
            return Err(Error::Multiline { field: "status" });
        }
        let st = ShardStatus {
            gist: gist.trim().to_string(),
            at: now(),
        };
        let path = self.status_path(id);
        fs::create_dir_all(path.parent().expect("has parent"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&st).expect("serializable"),
        )?;
        fs::rename(tmp, path)?;
        Ok(st)
    }

    pub(crate) fn blob(&self, rev: &str, path: &str) -> Result<Option<(String, String)>> {
        let out = self.git.run(&["ls-tree", rev, "--", path])?;
        // "<mode> blob <sha>\t<path>"
        Ok(out.lines().next().and_then(|l| {
            let (meta, _) = l.split_once('\t')?;
            let mut it = meta.split_whitespace();
            let mode = it.next()?.to_string();
            let _kind = it.next()?;
            let sha = it.next()?.to_string();
            Some((mode, sha))
        }))
    }

    pub(crate) fn diff_names(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let out = self
            .git
            .run(&["diff-tree", "-r", "--no-commit-id", "--name-only", a, b])?;
        Ok(out.lines().map(String::from).collect())
    }

    /// One side of a conflict: from front matter for facts, else from git log.
    pub(crate) fn side(&self, rev: &str, path: &str) -> Result<Side> {
        let text = self.read_file_at(rev, path)?;
        if let (Some(t), Some(_)) = (&text, Address::from_home(path)) {
            if let Ok(f) = FactFile::parse(t, path) {
                return Ok(Side {
                    value: f.value,
                    author: f.front.author,
                    human: f.front.human,
                    updated: f.front.updated,
                    why: f.front.why,
                });
            }
        }
        let log = self.git.run(&[
            "log",
            "-1",
            "--format=%an%x00%aI%x00%(trailers:key=Shards-Human,valueonly,separator=)%x00%(trailers:key=Shards-Why,valueonly,separator=)",
            rev,
            "--",
            path,
        ])?;
        let mut parts = log.trim_end().split('\0');
        let mut next = || parts.next().unwrap_or("").trim().to_string();
        let (author, updated, human, why) = (next(), next(), next(), next());
        Ok(Side {
            value: text.unwrap_or_else(|| "(deleted)".to_string()),
            author,
            human: Some(human).filter(|h| !h.is_empty()),
            updated,
            why,
        })
    }

    pub(crate) fn render_record(
        &self,
        shard: &str,
        c: &Conflict,
        main_commit: &str,
        shard_commit: &str,
    ) -> String {
        let recorded = now();
        let mh = c.main.human.clone().unwrap_or_default();
        let sh = c.shard.human.clone().unwrap_or_default();
        let pairs: Vec<(&str, &str)> = vec![
            ("target", &c.target),
            ("path", &c.path),
            ("shard", shard),
            ("recorded", &recorded),
            ("main_commit", main_commit),
            ("main_value", &c.main.value),
            ("main_author", &c.main.author),
            ("main_human", &mh),
            ("main_updated", &c.main.updated),
            ("main_why", &c.main.why),
            ("shard_commit", shard_commit),
            ("shard_value", &c.shard.value),
            ("shard_author", &c.shard.author),
            ("shard_human", &sh),
            ("shard_updated", &c.shard.updated),
            ("shard_why", &c.shard.why),
        ];
        let take_hint = if Address::from_home(&c.path).is_some() {
            c.target.clone()
        } else {
            c.path.clone()
        };
        let body = format!(
            "# Conflict: {t}\n\n\
             Shard `{shard}` and main both changed `{p}` since the shard's base.\n\
             Main still has its own value. A human decides:\n\n\
             ```sh\nmem resolve-conflict {take_hint} --take shard|main|<value> --why \"...\"\n```\n\n\
             | | main | shard `{shard}` |\n| --- | --- | --- |\n\
             | value | {mv} | {sv} |\n| author | {ma} | {sa} |\n| human | {mh} | {sh} |\n\
             | updated | {mu} | {su} |\n| why | {mw} | {sw} |\n",
            t = c.target,
            p = c.path,
            mv = c.main.value.replace('\n', " ⏎ "),
            sv = c.shard.value.replace('\n', " ⏎ "),
            ma = c.main.author,
            sa = c.shard.author,
            mu = c.main.updated,
            su = c.shard.updated,
            mw = c.main.why,
            sw = c.shard.why,
        );
        render_front(&pairs, &body)
    }

    /// Merge `shard/<id>` into main under the lock(s). See the module docs.
    pub fn dinit(&self, id: &str, all_or_nothing: bool) -> Result<DinitReport> {
        let main = self.main()?;
        let meta = main.shard_meta(id)?;
        let mut report = DinitReport {
            shard: id.to_string(),
            ..Default::default()
        };
        let human = meta.human.as_deref();
        let shard_lock = main.lock_shard(id, id, human)?;

        // Which paths might we touch? (Stable: only dinit moves the base,
        // and we hold the shard lock.)
        let tip = main
            .git
            .run(&["rev-parse", &meta.branch])?
            .trim()
            .to_string();
        let base0 = main
            .git
            .run(&["merge-base", "main", &tip])?
            .trim()
            .to_string();
        let shard_paths = main.diff_names(&base0, &tip)?;
        let mut lock_paths: Vec<String> = shard_paths
            .iter()
            .filter(|p| !p.starts_with("soul/"))
            .cloned()
            .collect();
        lock_paths.push(format!("conflicts/{id}/x"));
        lock_paths.push(crate::proposal::path_of("p0"));
        let guards = main.lock_main(&lock_paths, id, human)?;
        let _git = main.git_mutex()?;

        let main_head = main.git.run(&["rev-parse", "main"])?.trim().to_string();
        let base = main
            .git
            .run(&["merge-base", &main_head, &tip])?
            .trim()
            .to_string();
        let main_changed: BTreeSet<String> =
            main.diff_names(&base, &main_head)?.into_iter().collect();
        let shard_changed = main.diff_names(&base, &tip)?;

        let mut apply: Vec<String> = Vec::new();
        let mut propose: Vec<String> = Vec::new();
        let mut proposals: BTreeSet<String> = meta.soul_pending.iter().cloned().collect();
        for p in &shard_changed {
            let top = p.split('/').next().unwrap_or("");
            let same = main.blob(&tip, p)? == main.blob(&main_head, p)?;
            match top {
                "soul" => {
                    proposals.insert(p.clone());
                }
                "facts" | "people" | "notes" => {
                    if same {
                        report.unchanged.push(p.clone());
                    } else if main_changed.contains(p) {
                        report.conflicts.push(Conflict {
                            target: target_of(p),
                            path: p.clone(),
                            record: record_path(id, p),
                            main: main.side(&main_head, p)?,
                            shard: main.side(&tip, p)?,
                        });
                    } else if top == "notes" {
                        // Notes are never facts: merged with no review.
                        apply.push(p.clone());
                    } else {
                        // The core decides facts and people.
                        propose.push(p.clone());
                    }
                }
                _ => report.skipped.push(p.clone()),
            }
        }
        // A proposal is pending only while the shard's soul differs from main's.
        let mut pending = Vec::new();
        for p in proposals {
            if main.blob(&tip, &p)? != main.blob(&main_head, &p)? {
                pending.push(p);
            }
        }
        report.proposals = pending.clone();

        if all_or_nothing && !report.conflicts.is_empty() {
            report.aborted = true;
            return Ok(report);
        }
        let proposal = if propose.is_empty() {
            None
        } else {
            Some(main.build_proposal(id, human, &main_head, &base, &tip, &propose)?)
        };

        let tip_merged = main
            .git
            .probe(&["merge-base", "--is-ancestor", &tip, &main_head])?;
        if !tip_merged {
            // Build the merge tree in a private index.
            let tmp_dir = main.state_dir().join("tmp");
            fs::create_dir_all(&tmp_dir)?;
            let index = tmp_dir.join(format!("index-{id}-{}", std::process::id()));
            let _ = fs::remove_file(&index);
            let env = |_: ()| vec![("GIT_INDEX_FILE", index.to_string_lossy().to_string())];
            let idx = |args: &[&str]| {
                main.git.exec(
                    args,
                    Opts {
                        env: env(()),
                        ..Default::default()
                    },
                )
            };
            let result = (|| -> Result<String> {
                idx(&["read-tree", &main_head])?;
                for p in &apply {
                    match main.blob(&tip, p)? {
                        Some((mode, sha)) => {
                            idx(&[
                                "update-index",
                                "--add",
                                "--cacheinfo",
                                &format!("{mode},{sha},{p}"),
                            ])?;
                        }
                        None => {
                            idx(&["update-index", "--force-remove", "--", p])?;
                        }
                    }
                }
                if let Some(prop) = &proposal {
                    let text = Memory::proposal_text(prop);
                    let sha = main.git.exec(
                        &["hash-object", "-w", "--stdin"],
                        Opts {
                            stdin: Some(text.as_bytes()),
                            ..Default::default()
                        },
                    )?;
                    idx(&[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{},{}", sha.trim(), prop.path()),
                    ])?;
                }
                for c in &report.conflicts {
                    let text = main.render_record(id, c, &main_head, &tip);
                    let sha = main.git.exec(
                        &["hash-object", "-w", "--stdin"],
                        Opts {
                            stdin: Some(text.as_bytes()),
                            ..Default::default()
                        },
                    )?;
                    idx(&[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{},{}", sha.trim(), c.record),
                    ])?;
                }
                Ok(idx(&["write-tree"])?.trim().to_string())
            })();
            let _ = fs::remove_file(&index);
            let tree = result?;

            let mut summary = format!(
                "merged {}, proposed {}, conflicts {}, soul proposals {}, unchanged {}, skipped {}\n",
                apply.len(),
                propose.len(),
                report.conflicts.len(),
                pending.len(),
                report.unchanged.len(),
                report.skipped.len()
            );
            summary.push('\n');
            let mut trailers: Vec<(&str, String)> = vec![("Shard", id.to_string())];
            trailers.extend(apply.iter().map(|p| ("Merged", p.clone())));
            if let Some(prop) = &proposal {
                trailers.push(("Proposal-Id", prop.id.clone()));
            }
            trailers.extend(propose.iter().map(|p| ("Proposed", p.clone())));
            trailers.extend(
                report
                    .conflicts
                    .iter()
                    .map(|c| ("Conflict", c.path.clone())),
            );
            trailers.extend(pending.iter().map(|p| ("Proposal", p.clone())));
            let prov = Provenance {
                author: id.to_string(),
                human: meta.human.clone(),
                why: format!("dinit of shard {id}"),
                at: Some(now()),
                answers: None,
            };
            let msg = prov.message("dinit", id, &trailers);
            let msg = msg.replacen("\n\n", &format!("\n\n{summary}"), 1);
            let stamp = prov.stamp(&prov.timestamp());
            let commit = main
                .git
                .exec(
                    &[
                        "commit-tree",
                        &tree,
                        "-p",
                        &main_head,
                        "-p",
                        &tip,
                        "-F",
                        "-",
                    ],
                    Opts {
                        stamp: Some(&stamp),
                        stdin: Some(msg.as_bytes()),
                        ..Default::default()
                    },
                )?
                .trim()
                .to_string();
            for g in &guards {
                g.ensure_held()?;
            }
            main.git.run(&["merge", "--ff-only", "--quiet", &commit])?;
            report.commit = Some(commit);
        }
        report.merged = apply;
        if let Some(prop) = proposal {
            report.proposal = Some(prop.id);
            report.proposed = propose;
        }

        let mut meta = meta;
        meta.soul_pending = pending;
        meta.soul_commit = if meta.soul_pending.is_empty() {
            None
        } else {
            Some(tip.clone())
        };
        main.save_meta(&meta)?;
        drop(guards);

        let open_records = main
            .git
            .run(&["ls-files", "--", &format!("conflicts/{id}/")])?;
        if report.conflicts.is_empty() && meta.soul_pending.is_empty() {
            if open_records.trim().is_empty() {
                if main.close_shard(&meta, &mut report)? {
                    // Run from inside the shard's own session: that session
                    // is waiting on this very command, so it can't take
                    // `/exit`. Everything is done; let go of the shard lock
                    // and end the session, and this process with it.
                    drop(shard_lock);
                    main.shard_session_end_from_inside(&meta);
                }
            } else {
                report.kept_open = Some(format!(
                    "it still has open conflicts under conflicts/{id}/ (settle with mem resolve-conflict)"
                ));
            }
        }
        Ok(report)
    }

    /// After a clean dinit: remove the worktree, maybe the branch, and the
    /// shard's local state. A worktree with uncommitted changes is kept.
    /// True if the shard closed but its session is still to be ended,
    /// because this dinit runs inside that session (see `dinit`).
    fn close_shard(&self, meta: &ShardMeta, report: &mut DinitReport) -> Result<bool> {
        // A shard is a session in a pane: end it before its worktree goes.
        // Unless this dinit runs in that very session (its agent ran `mem
        // dinit`): ending the session ends this process, so close first.
        let from_inside = self.inside_shard_pane(meta);
        if !from_inside {
            report.session_stopped = self.shard_session_end(meta);
        }
        if meta.worktree.exists() {
            let wt = meta.worktree.to_string_lossy().to_string();
            if let Err(e) = self.git.run(&["worktree", "remove", &wt]) {
                report.kept_open = Some(format!(
                    "worktree {} was not removed (uncommitted changes?): {e}",
                    meta.worktree.display()
                ));
                return Ok(false);
            }
        }
        if !self.config.shards.keep_branches {
            self.git.run(&["branch", "-d", &meta.branch])?;
            report.branch_deleted = true;
        }
        let _ = fs::remove_file(self.shards_dir().join(format!("{}.json", meta.id)));
        let _ = fs::remove_file(self.status_path(&meta.id));
        self.forget_activity(&meta.id);
        report.closed = true;
        report.session_stopped |= from_inside;
        Ok(from_inside)
    }

    /// Open conflict records on main for `target` (an address, or a
    /// repo path ending in `.md`): `(shard id, record path)`.
    pub fn conflicts_for(&self, target: &str) -> Result<Vec<(String, String)>> {
        let main = self.main()?;
        let suffix = if target.ends_with(".md") {
            format!("@/{target}")
        } else {
            format!("{}.md", Address::parse(target)?)
        };
        let out = main.git.run(&["ls-files", "--", "conflicts/"])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("conflicts/")?;
                let (shard, tail) = rest.split_once('/')?;
                (tail == suffix).then(|| (shard.to_string(), l.to_string()))
            })
            .collect())
    }

    /// Settle a conflict on main. A person's pick is final: the chosen value
    /// goes to main (unless `main`) with no proposal review, the record is
    /// removed, and an incident (`incidents/i<n>.json`) is written, all in
    /// one commit. Needs the settler's name (`prov.human`) and a reason.
    pub fn resolve_conflict(
        &self,
        target: &str,
        take: &Take,
        shard: Option<&str>,
        prov: &Provenance,
    ) -> Result<Resolution> {
        let main = self.main()?;
        // Author rules are this worktree's: from a shard, the shard id, and
        // the settler defaults to the shard's human.
        let mut prov = prov.clone();
        if prov.author.trim().is_empty() && matches!(self.context, crate::memory::Context::Main) {
            prov.author = prov.human.clone().unwrap_or_default();
            if prov.author.trim().is_empty() {
                return Err(Error::MissingSettler);
            }
        }
        let prov = self.effective(&prov, true)?;
        if prov.human.as_deref().is_none_or(|h| h.trim().is_empty()) {
            return Err(Error::MissingSettler);
        }
        let mut found = main.conflicts_for(target)?;
        if let Some(s) = shard {
            found.retain(|(id, _)| id == s);
        }
        let (sid, record) = match found.len() {
            0 => return Err(Error::NoConflict(target.to_string())),
            1 => found.remove(0),
            _ => {
                return Err(Error::AmbiguousConflict {
                    target: target.to_string(),
                    shards: found.into_iter().map(|(s, _)| s).collect(),
                })
            }
        };
        let text = main
            .read_file_at("HEAD", &record)?
            .ok_or_else(|| Error::NoConflict(target.to_string()))?;
        let (pairs, _) = parse_front(&text, &record)?;
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let side = |pre: &str| IncidentSide {
            value: get(&format!("{pre}_value")),
            author: get(&format!("{pre}_author")),
            human: Some(get(&format!("{pre}_human"))).filter(|h| !h.is_empty()),
            updated: get(&format!("{pre}_updated")),
            why: get(&format!("{pre}_why")),
        };
        let (main_side, shard_side) = (side("main"), side("shard"));
        let path = get("path");
        let shard_value = shard_side.value.clone();
        let incident_lock = format!("{}/_", crate::incident::DIR);
        let trailers = vec![
            ("Target", target.to_string()),
            ("Shard", sid.clone()),
            ("Take", take.label().to_string()),
        ];
        let lock_paths = vec![path.clone(), record.clone(), incident_lock];
        // The incident id is only known under the lock.
        let mut incident_id = String::new();
        let commit = main.locked_commit(
            "resolve-conflict",
            target,
            &lock_paths,
            &prov,
            &trailers,
            |m, prov| {
                let mut writes: Vec<FileWrite> = vec![(record.clone(), None)];
                let value = match take {
                    Take::Main => None,
                    Take::Shard => Some(shard_value.clone()),
                    Take::Value(v) => Some(v.clone()),
                };
                if let Some(v) = &value {
                    if path.starts_with("soul/") {
                        m.require_admin(prov.human.as_deref(), "changing the soul on main")?;
                    }
                    let contents = match Address::from_home(&path) {
                        Some(a) => fact_file(&a, v, prov),
                        None => v.clone(),
                    };
                    writes.insert(0, (path.clone(), Some(contents)));
                }
                let human = prov.human.clone().unwrap_or_default();
                let mut settled = Settlement {
                    take: take.label().to_string(),
                    value: value.unwrap_or_else(|| main_side.value.clone()),
                    correct: match take {
                        Take::Shard => format!("shard {sid}"),
                        Take::Main => "main".to_string(),
                        Take::Value(_) => "neither".to_string(),
                    },
                    human: human.clone(),
                    by: prov.author.clone(),
                    why: prov.why.clone(),
                    at: prov.timestamp(),
                };
                if let (Take::Main, false) = (take, main_side.author.is_empty()) {
                    settled.correct = format!("main (by {})", main_side.author);
                }
                let mut shards = Vec::new();
                for s in [&main_side.author, &sid] {
                    if !s.is_empty() && !shards.contains(s) {
                        shards.push(s.clone());
                    }
                }
                let mut humans = Vec::new();
                for h in [
                    main_side.human.as_ref(),
                    shard_side.human.as_ref(),
                    Some(&human),
                ]
                .into_iter()
                .flatten()
                {
                    if !h.is_empty() && !humans.contains(h) {
                        humans.push(h.clone());
                    }
                }
                let id = m.next_record_id("HEAD", crate::incident::DIR, crate::incident::PREFIX)?;
                let inc = Incident {
                    format: crate::incident::FORMAT,
                    summary: crate::incident::summary(target, &sid, &main_side.author, &settled),
                    id: id.clone(),
                    target: target.to_string(),
                    path: path.clone(),
                    conflict_record: record.clone(),
                    conflict_recorded: get("recorded"),
                    shard: sid.clone(),
                    main: main_side.clone(),
                    shard_side: shard_side.clone(),
                    shards,
                    humans,
                    settled,
                    notes: Vec::new(),
                };
                writes.push((inc.path(), Some(crate::record::render(&inc))));
                incident_id = id;
                Ok(writes)
            },
        )?;
        Ok(Resolution {
            commit,
            incident: incident_id,
        })
    }

    /// An admin approves a shard's pending soul proposal: the shard's soul
    /// files are written to main as one commit.
    pub fn soul_approve(&self, id: &str, prov: &Provenance) -> Result<String> {
        let main = self.main()?;
        let mut prov = prov.clone();
        if prov.author.trim().is_empty() {
            prov.author = prov.human.clone().unwrap_or_default();
        }
        if prov.why.trim().is_empty() {
            prov.why = format!("approved soul change from shard {id}");
        }
        main.require_admin(prov.human.as_deref(), "approving a soul change")?;
        let prov = main.effective(&prov, true)?;
        let mut meta = main.shard_meta(id)?;
        let from = match (&meta.soul_commit, meta.soul_pending.is_empty()) {
            (Some(c), false) => c.clone(),
            _ => return Err(Error::NothingToApprove(id.to_string())),
        };
        let paths = meta.soul_pending.clone();
        let trailers = vec![
            ("Shard", id.to_string()),
            ("Approved-By", prov.human.clone().unwrap_or_default()),
            ("From-Commit", from.clone()),
        ];
        let commit = main.locked_commit("soul-approve", id, &paths, &prov, &trailers, |m, _| {
            paths
                .iter()
                .map(|p| Ok((p.clone(), m.read_file_at(&from, p)?)))
                .collect()
        })?;
        meta.soul_pending.clear();
        meta.soul_commit = None;
        main.save_meta(&meta)?;
        Ok(commit)
    }
}
