// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem`: the one door into a shards memory. A thin layer over `shards-core`.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use shards_core::board::{human_age, Board};
use shards_core::coresession::summarize_usage;
use shards_core::coresession::Landed;
use shards_core::shardsession::ShardLaunch;
use shards_core::{
    resolve, sample, Address, CoreStatus, DinitReport, Memory, NewQuirk, Owner, PassReport,
    Proposal, Provenance, Quirk, QuirkState, Rank, ResolveOptions, ShardMeta, ShardStart, Side,
    StartReport, Take, UsageEntry, Verdict, WakeOutcome,
};

#[derive(Parser)]
#[command(name = "mem", version, about = "Read and write a shards memory")]
struct Cli {
    /// The memory repository to use.
    #[arg(
        long,
        short = 'C',
        global = true,
        env = "SHARDS_MEMORY",
        default_value = "."
    )]
    memory: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Who {
    /// Shard / session id recorded as the author.
    #[arg(long, env = "SHARDS_AUTHOR")]
    author: Option<String>,
    /// The human at the keyboard.
    #[arg(long, env = "SHARDS_HUMAN")]
    human: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new, empty memory repository.
    Init {
        dir: PathBuf,
        #[command(flatten)]
        who: Who,
    },
    /// Print a fact's current value and its home path.
    Get { address: String },
    /// Create a NEW fact (refuses an existing address; use supersede to change one).
    Set {
        address: String,
        /// The value; `-` reads it from stdin.
        value: String,
        #[arg(long)]
        why: Option<String>,
        /// The open question this answers (e.g. ask2), from a shard.
        #[arg(long, value_name = "QUESTION")]
        answers: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Replace the value of an existing fact (old value stays in history).
    Supersede {
        address: String,
        /// The value; `-` reads it from stdin.
        value: String,
        #[arg(long)]
        why: Option<String>,
        /// The open question this answers (e.g. ask2), from a shard.
        #[arg(long, value_name = "QUESTION")]
        answers: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Every version of a fact, newest first, and any shard's drafts of it.
    History { address: String },
    /// Replace every {{fact:<address>}} pointer with the current value.
    Resolve {
        /// File to read; `-` or omitted reads stdin.
        file: Option<PathBuf>,
        /// Add the fact's address after each value.
        #[arg(long)]
        annotate: bool,
    },
    /// Build a small FAKE flock memory for tests.
    GenSample {
        dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Write a notes/, people/ or soul/ markdown file (a soul change on main
    /// needs an admin --human). With --for, the note goes in notes/<person>/.
    Put {
        path: String,
        /// File to read the contents from; `-` or omitted reads stdin.
        file: Option<PathBuf>,
        #[arg(long)]
        why: Option<String>,
        /// Whose note it is: writes notes/<person>/<file>. Without it, a
        /// notes/<file>.md path is a shared note.
        #[arg(long = "for", value_name = "PERSON")]
        for_person: Option<String>,
        /// The open question this answers (e.g. ask2), from a shard.
        #[arg(long, value_name = "QUESTION")]
        answers: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Open, list and describe shards.
    #[command(subcommand)]
    Shard(ShardCmd),
    /// Merge shard/<id> into main under the lock.
    Dinit {
        #[arg(long)]
        id: String,
        /// Merge nothing if anything conflicts.
        #[arg(long)]
        all_or_nothing: bool,
    },
    /// Settle a conflict: a person's pick is final and goes straight to main.
    /// Needs the person's name (--human) and --why; writes an incident.
    ResolveConflict {
        /// A fact address, or a repo path ending in .md (people/, notes/).
        target: String,
        /// `shard`, `main`, or a literal new value.
        #[arg(long)]
        take: String,
        /// Why that side is right (required; kept in the incident).
        #[arg(long)]
        why: Option<String>,
        /// Which shard's conflict, if several shards conflict on it.
        #[arg(long)]
        shard: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Soul proposals.
    #[command(subcommand)]
    Soul(SoulCmd),
    /// A shard's fact and people changes, waiting for the core: list, show,
    /// and accept, reject or hold each change.
    #[command(subcommand)]
    Proposal(ProposalCmd),
    /// Settled conflicts: list, show, and add a note (nothing is ever removed).
    #[command(subcommand)]
    Incident(IncidentCmd),
    /// Open questions from held changes, for the next person who can answer.
    #[command(subcommand)]
    Question(QuestionCmd),
    /// What every open shard is doing, read from git and its transcript.
    Board {
        /// Machine-readable output (format documented in the README).
        #[arg(long)]
        json: bool,
    },
    /// Evaluate [triggers] every [intervals] watch_poll_secs; wake the core when one fires.
    Watch {
        /// Run one pass and exit (for a timer or cron).
        #[arg(long)]
        once: bool,
    },
    /// The reasoning core's view.
    #[command(subcommand)]
    Core(CoreCmd),
    /// Notes: shared ones at the top of notes/, a person's own in notes/<person>/.
    #[command(subcommand)]
    Notes(NotesCmd),
    /// Known quirks: what's weird about a tool right now. Written straight
    /// to main from any shard, visible to every shard at once.
    #[command(subcommand)]
    Quirk(QuirkCmd),
    /// Test helpers.
    #[command(subcommand, hide = true)]
    Debug(DebugCmd),
}

#[derive(Subcommand)]
enum ShardCmd {
    /// Create shard/<id> from main in its own worktree; prints the path.
    Open {
        #[arg(long)]
        id: String,
        #[arg(long, env = "SHARDS_HUMAN")]
        human: Option<String>,
    },
    /// Open a shard and start its session in its own tmux pane (blank: no
    /// first message). With --resume, bring an open shard's pinned session back.
    Start {
        /// The shard id (default: the first free s<n>). Required with --resume.
        #[arg(long)]
        id: Option<String>,
        #[arg(long, env = "SHARDS_HUMAN")]
        human: Option<String>,
        /// Resume the shard's pinned session (after mem shard stop).
        #[arg(long)]
        resume: bool,
    },
    /// End a shard's session cleanly; keeps its branch, worktree and pinned session.
    Stop {
        #[arg(long)]
        id: String,
    },
    /// Attach to a shard's pane (prints the command when not on a terminal).
    Attach {
        #[arg(long)]
        id: String,
        /// Only print the tmux command.
        #[arg(long)]
        print: bool,
    },
    /// Type one message into a shard's pane and confirm its session got it.
    Send {
        #[arg(long)]
        id: String,
        /// The message; `-` reads stdin. Line breaks are folded into one line.
        text: String,
    },
    /// Open shards: human, pane, session, base, last commit, status line.
    List {
        #[arg(long)]
        json: bool,
    },
    /// What a shard should know at the start of a session: who it is with,
    /// and the open questions this person may answer.
    Context {
        /// The shard (default: this pane's shard).
        #[arg(long, env = "SHARDS_SHARD")]
        id: Option<String>,
    },
    /// Set a shard's one-line status ("working on X").
    Status {
        #[arg(long)]
        id: String,
        gist: String,
    },
}

#[derive(Subcommand)]
enum SoulCmd {
    /// Approve a shard's soul proposal (admins only, from shards.toml).
    Approve {
        shard: String,
        #[arg(long, env = "SHARDS_HUMAN")]
        human: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
        #[arg(long, default_value = "")]
        why: String,
    },
}

#[derive(clap::Args)]
struct Decide {
    /// The proposal, e.g. p3.
    id: String,
    /// Decide only this change (default: every change still open).
    #[arg(long)]
    change: Option<usize>,
    /// Why: required, and logged with the decision for good.
    #[arg(long)]
    why: Option<String>,
    /// The reviewer (default: core).
    #[arg(long, env = "SHARDS_AUTHOR")]
    author: Option<String>,
    /// A human deciding (required for a held change).
    #[arg(long, env = "SHARDS_HUMAN")]
    human: Option<String>,
}

#[derive(Subcommand)]
enum ProposalCmd {
    /// Proposals with a change still pending or held (--all: every one).
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Every change in a proposal, its value, reason and status, and every decision.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Accept: write the change to main. With --value, accept that value
    /// instead (a fix; the record keeps both).
    Accept {
        #[command(flatten)]
        d: Decide,
        /// The fixed value to accept instead of the proposed one; `-` reads stdin.
        #[arg(long)]
        value: Option<String>,
    },
    /// Reject: main stays as it is.
    Reject {
        #[command(flatten)]
        d: Decide,
    },
    /// Hold for a human: main stays as it is until a human decides. Needs
    /// --ask: the question for the next person who can answer it.
    Hold {
        #[command(flatten)]
        d: Decide,
        /// What to ask the next person, e.g. "Mike mentioned he replaced the
        /// spindle, but only in passing. Would you confirm it?"
        #[arg(long)]
        ask: Option<String>,
    },
}

#[derive(Subcommand)]
enum IncidentCmd {
    /// Every incident, newest first.
    List {
        #[arg(long)]
        json: bool,
    },
    /// One incident: both sides, who settled it and why, and any notes.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a note to an incident, with a reason (the core's, usually).
    Note {
        id: String,
        /// The note; `-` reads stdin.
        text: String,
        #[arg(long)]
        why: Option<String>,
        #[command(flatten)]
        who: Who,
    },
}

#[derive(Subcommand)]
enum QuestionCmd {
    /// Open questions (--all: closed ones too).
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// One question: what to ask, who said it, and everything since.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// The person asked doesn't know: logged, and the question stays open.
    DontKnow {
        id: String,
        /// What they said.
        #[arg(long)]
        why: Option<String>,
        #[command(flatten)]
        who: Who,
    },
}

#[derive(Subcommand)]
enum CoreCmd {
    /// Print `mem board --json` plus the pending events, as one JSON document.
    Context,
    /// Start the core in its own tmux pane (a no-op if it is running).
    Start {
        /// Resume the pinned session instead of starting a fresh one.
        #[arg(long)]
        resume: bool,
    },
    /// Stop the core deliberately (the watchdog leaves it stopped).
    Stop,
    /// Pane, command, pinned session, last wake, watchdog and keep-warm.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Attach to the core's pane (prints the command when not on a terminal).
    Attach {
        /// Only print the tmux command.
        #[arg(long)]
        print: bool,
    },
    /// One watchdog pass (restore a dead core, resume its pinned session),
    /// then keep-warm.
    Check,
    /// The core has handled the pending events (up to --upto).
    Ack {
        /// The newest event id handled (default: the newest it was woken for).
        #[arg(long)]
        upto: Option<u64>,
    },
    /// Hand off to a fresh session whose first message carries the baton.
    Rinit {
        /// The baton file; `-` reads stdin.
        #[arg(long)]
        baton: PathBuf,
    },
    /// Token usage of pings and wakes, by gap length, warm and cold.
    Usage {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum QuirkCmd {
    /// Post a quirk (the report time is set automatically). Duplicates are fine.
    Add {
        #[arg(long)]
        tool: String,
        #[arg(long)]
        quirk: String,
        /// Who reported it.
        #[arg(long)]
        by: String,
        /// Free-text details; `-` reads stdin.
        #[arg(long)]
        details: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
    },
    /// Open quirks (all of them with --all, including fixed and archived).
    List {
        #[arg(long)]
        tool: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Every version of a quirk's status, newest first.
    History { id: String },
    /// Set a quirk's status in words; it is open again. The old status stays in history.
    Update {
        id: String,
        #[arg(long)]
        status: String,
        #[arg(long)]
        by: String,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
    },
    /// Mark a quirk fixed: status becomes "fixed by PERSON".
    Fix {
        id: String,
        #[arg(long)]
        by: String,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
    },
    /// A quirk came up again: resets its archive clock (unarchives it).
    Mention {
        id: String,
        #[arg(long, env = "SHARDS_HUMAN")]
        by: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
    },
    /// Archive fixed quirks not mentioned for N days. Open quirks never archive;
    /// nothing is deleted.
    Archive {
        /// Default: [intervals] quirk_archive_days in shards.toml (90).
        #[arg(long)]
        days: Option<f64>,
        #[arg(long, env = "SHARDS_HUMAN")]
        by: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
    },
}

#[derive(Subcommand)]
enum NotesCmd {
    /// Find notes containing a word: the person's own first, then shared
    /// notes, then everyone else's. Notes are never facts.
    Find {
        word: String,
        #[arg(long = "for", value_name = "PERSON")]
        for_person: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Take the main lock and hold it (for crash-recovery tests).
    HoldLock {
        #[arg(long, default_value_t = 60.0)]
        secs: f64,
        #[arg(long, default_value = "debug-holder")]
        holder: String,
    },
}

fn short(c: &str) -> &str {
    &c[..c.len().min(12)]
}

fn side_line(s: &Side) -> String {
    format!(
        "{}  (by {}{}, {}, why: {})",
        s.value.replace('\n', " ⏎ "),
        s.author,
        s.human
            .as_deref()
            .map(|h| format!(" / {h}"))
            .unwrap_or_default(),
        s.updated,
        s.why
    )
}

fn print_dinit(out: &mut impl Write, r: &DinitReport) -> io::Result<()> {
    if r.aborted {
        writeln!(
            out,
            "dinit {} ABORTED (--all-or-nothing): {} conflict(s); main untouched",
            r.shard,
            r.conflicts.len()
        )?;
    } else {
        match &r.commit {
            Some(c) => writeln!(out, "dinit {} -> main at {}", r.shard, short(c))?,
            None => writeln!(out, "dinit {}: nothing new to merge", r.shard)?,
        }
    }
    for p in &r.merged {
        writeln!(out, "  merged    {p}")?;
    }
    for p in &r.proposed {
        writeln!(
            out,
            "  PROPOSED  {p}  ({}: waits for the core, not on main yet)",
            r.proposal.as_deref().unwrap_or("?")
        )?;
    }
    for p in &r.unchanged {
        writeln!(out, "  same      {p}")?;
    }
    for c in &r.conflicts {
        writeln!(out, "  CONFLICT  {}  (record: {})", c.target, c.record)?;
        writeln!(out, "      main:  {}", side_line(&c.main))?;
        writeln!(out, "      shard: {}", side_line(&c.shard))?;
    }
    for p in &r.proposals {
        writeln!(
            out,
            "  PROPOSAL  {p}  (needs a human admin: mem soul approve {} --human <admin>)",
            r.shard
        )?;
    }
    for p in &r.skipped {
        writeln!(out, "  skipped   {p}  (dinit never merges this)")?;
    }
    if r.session_stopped {
        writeln!(out, "shard {}'s session was stopped first", r.shard)?;
    }
    if r.closed {
        writeln!(
            out,
            "shard {} closed: worktree removed, branch {}",
            r.shard,
            if r.branch_deleted { "deleted" } else { "kept" }
        )?;
    } else if let Some(why) = &r.kept_open {
        writeln!(out, "shard {} left open: {why}", r.shard)?;
    } else if !r.aborted {
        writeln!(
            out,
            "shard {} stays open until its conflicts/proposals are settled",
            r.shard
        )?;
    }
    Ok(())
}

fn print_board(out: &mut impl Write, b: &Board) -> io::Result<()> {
    if b.shards.is_empty() {
        writeln!(out, "no open shards")?;
    }
    for s in &b.shards {
        let others = s.in_flight.len() - s.facts_in_flight;
        writeln!(
            out,
            "{}  {}  {}  pane {}  last activity {} ago ({})  in flight: {} fact(s){}",
            s.id,
            s.human.as_deref().unwrap_or("-"),
            s.state.as_str(),
            s.pane.as_str(),
            human_age(s.age_secs),
            s.last_activity_from,
            s.facts_in_flight,
            if others > 0 {
                format!(", {others} other")
            } else {
                String::new()
            }
        )?;
        if let Some(g) = &s.gist {
            writeln!(out, "    gist:   {g}")?;
        }
        if !s.in_flight.is_empty() {
            let items: Vec<String> = s
                .in_flight
                .iter()
                .map(|f| {
                    format!(
                        "{}{}",
                        f.address.as_deref().unwrap_or(&f.path),
                        if f.uncommitted { "*" } else { "" }
                    )
                })
                .collect();
            writeln!(out, "    flight: {}  (* uncommitted)", items.join(", "))?;
        }
        for a in &s.recent_actions {
            let age = chrono_age(&b.generated, &a.at);
            if a.summary.is_empty() {
                writeln!(out, "    {age:>4} ago  {}", a.tool)?;
            } else {
                writeln!(out, "    {age:>4} ago  {}  {}", a.tool, a.summary)?;
            }
        }
    }
    if !b.overlaps.is_empty() {
        writeln!(out, "\noverlaps:")?;
        for o in &b.overlaps {
            writeln!(
                out,
                "  {:<12}  {}  shards {}{}",
                o.kind.as_str(),
                o.target,
                o.shards.join(", "),
                if o.humans.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", o.humans.join(", "))
                }
            )?;
        }
    }
    if !b.conflicts.is_empty() {
        writeln!(out, "\nopen conflicts:")?;
        for c in &b.conflicts {
            writeln!(out, "  {}  from {}  ({})", c.target, c.shard, c.record)?;
        }
    }
    if !b.soul_proposals.is_empty() {
        writeln!(out, "\nsoul proposals:")?;
        for p in &b.soul_proposals {
            writeln!(
                out,
                "  {} ({})  {}",
                p.shard,
                p.human.as_deref().unwrap_or("-"),
                p.paths.join(", ")
            )?;
        }
    }
    if !b.proposals.is_empty() {
        writeln!(out, "\nproposals (mem proposal show <id>):")?;
        for p in &b.proposals {
            let pending: Vec<&str> = p
                .changes
                .iter()
                .filter(|c| c.status == "pending" || c.status == "held")
                .map(|c| c.target.as_str())
                .collect();
            writeln!(
                out,
                "  {}  from {} ({})  {} pending, {} held  {}",
                p.id,
                p.shard,
                p.human.as_deref().unwrap_or("-"),
                p.pending,
                p.held,
                pending.join(", ")
            )?;
        }
    }
    if !b.questions.is_empty() {
        writeln!(out, "\nopen questions (mem question show <id>):")?;
        for q in &b.questions {
            writeln!(
                out,
                "  {}  {}  ({}, said by {}{})  {}",
                q.id,
                q.target,
                q.change,
                q.said_by,
                if q.dont_know > 0 {
                    format!(", {} didn't know", q.dont_know)
                } else {
                    String::new()
                },
                q.ask
            )?;
        }
    }
    if !b.incidents.is_empty() {
        writeln!(out, "\nrecent incidents (mem incident show <id>):")?;
        for i in &b.incidents {
            writeln!(
                out,
                "  {}  {}  {}{}",
                i.id,
                i.settled_at,
                i.summary,
                if i.notes > 0 {
                    format!("  ({} note(s))", i.notes)
                } else {
                    String::new()
                }
            )?;
        }
    }
    Ok(())
}

fn print_drafts(out: &mut impl Write, p: &Proposal, c: &shards_core::Change) -> io::Result<()> {
    writeln!(
        out,
        "  drafts: shard {}'s earlier versions in {}#{}, oldest first (never on main):",
        p.shard, p.id, c.n
    )?;
    for d in &c.drafts {
        writeln!(
            out,
            "    {}  {}  draft: {}\n      why: {}",
            d.updated,
            short(&d.commit),
            d.value
                .as_deref()
                .unwrap_or("(removed)")
                .replace('\n', " ⏎ "),
            d.why
        )?;
    }
    Ok(())
}

fn print_incident(out: &mut impl Write, i: &shards_core::Incident) -> io::Result<()> {
    writeln!(out, "{}  {}", i.id, i.summary)?;
    writeln!(out, "  target:   {}  [{}]", i.target, i.path)?;
    writeln!(
        out,
        "  conflict: {} (recorded {})",
        i.conflict_record, i.conflict_recorded
    )?;
    writeln!(out, "  shards:   {}", i.shards.join(", "))?;
    writeln!(out, "  humans:   {}", i.humans.join(", "))?;
    for (label, s) in [
        ("main", &i.main),
        (&format!("shard {}", i.shard)[..], &i.shard_side),
    ] {
        writeln!(out, "\n  {label}:")?;
        writeln!(out, "    value:   {}", s.value.replace('\n', " ⏎ "))?;
        writeln!(
            out,
            "    by:      {}{}  ({})",
            s.author,
            s.human
                .as_deref()
                .map(|h| format!(" / {h}"))
                .unwrap_or_default(),
            s.updated
        )?;
        writeln!(out, "    why:     {}", s.why)?;
    }
    let st = &i.settled;
    writeln!(
        out,
        "\n  settled:  {} took {} ({} was correct) at {}",
        st.human, st.take, st.correct, st.at
    )?;
    writeln!(out, "    value:   {}", st.value.replace('\n', " ⏎ "))?;
    writeln!(out, "    why:     {}", st.why)?;
    if !i.notes.is_empty() {
        writeln!(out, "\n  notes:")?;
        for n in &i.notes {
            writeln!(
                out,
                "    {}  {}{}: {}\n      why: {}",
                n.at,
                n.by,
                n.human
                    .as_deref()
                    .map(|h| format!(" / {h}"))
                    .unwrap_or_default(),
                n.note.replace('\n', " ⏎ "),
                n.why
            )?;
        }
    }
    Ok(())
}

fn print_question(out: &mut impl Write, mem: &Memory, q: &shards_core::Question) -> io::Result<()> {
    writeln!(out, "{}  [{}]  about {}", q.id, q.status.as_str(), q.target)?;
    writeln!(out, "  ask:       {}", q.ask)?;
    writeln!(
        out,
        "  held:      {} by {}: {}",
        q.held_change(),
        q.held_by,
        q.held_why
    )?;
    writeln!(
        out,
        "  said by:   {} (shard {}): {}",
        q.said_by_human.as_deref().unwrap_or("-"),
        q.said_by_shard,
        q.value
            .as_deref()
            .unwrap_or("(removal)")
            .replace('\n', " ⏎ ")
    )?;
    writeln!(out, "  their why: {}", q.said_why)?;
    // Answers still waiting for the core.
    if let Ok(ps) = mem.proposals() {
        for p in ps {
            for c in p.changes.iter().filter(|c| c.status.is_open()) {
                if c.answers.as_deref() == Some(q.id.as_str()) {
                    writeln!(
                        out,
                        "  answer waiting for the core: {}#{} from {} ({}): {}",
                        p.id,
                        c.n,
                        p.shard,
                        p.human.as_deref().unwrap_or("-"),
                        c.value
                            .as_deref()
                            .unwrap_or("(removal)")
                            .replace('\n', " ⏎ ")
                    )?;
                }
            }
        }
    }
    if !q.log.is_empty() {
        writeln!(out, "\n  since:")?;
        for a in &q.log {
            writeln!(
                out,
                "    {}  {}  {}{}{}: {}",
                a.at,
                a.kind,
                a.by,
                a.human
                    .as_deref()
                    .map(|h| format!(" / {h}"))
                    .unwrap_or_default(),
                a.change
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default(),
                a.why
            )?;
        }
    }
    if let Some(c) = &q.closed {
        writeln!(
            out,
            "\n  closed {} by {} ({}), {}{}: {}",
            c.at,
            c.by_change,
            c.action,
            c.by,
            c.human
                .as_deref()
                .map(|h| format!(" / {h}"))
                .unwrap_or_default(),
            c.why
        )?;
    }
    Ok(())
}

fn print_proposal(out: &mut impl Write, p: &Proposal) -> io::Result<()> {
    writeln!(
        out,
        "{}  from shard {} ({})  made {}",
        p.id,
        p.shard,
        p.human.as_deref().unwrap_or("no human"),
        p.created
    )?;
    let one = |v: &Option<String>| match v {
        Some(v) => v.replace('\n', " ⏎ "),
        None => "(none)".to_string(),
    };
    for c in &p.changes {
        writeln!(out)?;
        writeln!(
            out,
            "  #{}  {}  {}  [{}]",
            c.n,
            c.op.as_str(),
            c.target(),
            c.status.as_str()
        )?;
        writeln!(out, "      proposed: {}", one(&c.value))?;
        writeln!(out, "      main had: {}", one(&c.main_value))?;
        writeln!(
            out,
            "      why:      {}",
            if c.why.is_empty() {
                "(no reason given)"
            } else {
                &c.why
            }
        )?;
        if let Some(v) = &c.accepted_value {
            if Some(v) != c.value.as_ref() {
                writeln!(out, "      accepted: {} (fixed)", v.replace('\n', " ⏎ "))?;
            }
        }
        if let Some(r) = &c.conflict_record {
            writeln!(out, "      conflict: {r} (mem resolve-conflict)")?;
        }
        if let Some(q) = &c.answers {
            writeln!(out, "      answers:  {q} (mem question show {q})")?;
        }
        if let Some(q) = &c.question {
            writeln!(out, "      question: {q} (mem question show {q})")?;
        }
        if let Some(a) = &c.answered_by {
            writeln!(out, "      answered by: {a}")?;
        }
        for (i, d) in c.drafts.iter().enumerate() {
            writeln!(
                out,
                "      draft {}: {}  (why: {})",
                i + 1,
                d.value
                    .as_deref()
                    .unwrap_or("(removed)")
                    .replace('\n', " ⏎ "),
                d.why
            )?;
        }
    }
    if !p.log.is_empty() {
        writeln!(out, "\n  decisions:")?;
        for d in &p.log {
            writeln!(
                out,
                "    {}  #{} {}  by {}{}: {}{}",
                d.at,
                d.change,
                d.action,
                d.by,
                d.human
                    .as_deref()
                    .map(|h| format!(" / {h}"))
                    .unwrap_or_default(),
                d.why,
                d.value
                    .as_deref()
                    .map(|v| format!("  (value: {})", v.replace('\n', " ⏎ ")))
                    .unwrap_or_default()
            )?;
            if let Some(q) = &d.question {
                writeln!(out, "      question: {q}")?;
            }
        }
    }
    let open: Vec<String> = p
        .changes
        .iter()
        .filter(|c| c.status.is_open())
        .map(|c| c.n.to_string())
        .collect();
    if !open.is_empty() {
        writeln!(
            out,
            "\n  open: #{}  (mem proposal accept|reject|hold {} [--change N] --why \"...\")",
            open.join(", #"),
            p.id
        )?;
    }
    Ok(())
}

/// Age of RFC 3339 `at` relative to `now`, e.g. "4m".
fn chrono_age(now: &str, at: &str) -> String {
    use chrono::DateTime;
    match (
        DateTime::parse_from_rfc3339(now),
        DateTime::parse_from_rfc3339(at),
    ) {
        (Ok(n), Ok(a)) => human_age((n - a).num_seconds().max(0)),
        _ => "?".to_string(),
    }
}

fn print_pass(out: &mut impl Write, r: &PassReport) -> io::Result<()> {
    for e in &r.fired {
        writeln!(out, "event {} {}: {}", e.id, e.kind.as_str(), e.summary)?;
    }
    match &r.wake {
        WakeOutcome::Idle => {}
        WakeOutcome::NoCommand => {
            if !r.fired.is_empty() {
                writeln!(
                    out,
                    "{} event(s) pending; no [wake] command set, nothing run",
                    r.pending
                )?;
            }
        }
        WakeOutcome::Cooldown { until } => {
            if !r.fired.is_empty() {
                writeln!(
                    out,
                    "{} event(s) pending; cooldown until {until}",
                    r.pending
                )?;
            }
        }
        WakeOutcome::Busy => writeln!(out, "another wake is running; events stay pending")?,
        WakeOutcome::Delivered { events, secs } => {
            writeln!(out, "woke the core: {events} event(s) in {secs:.1}s")?
        }
        WakeOutcome::Failed { events, reason } => writeln!(
            out,
            "wake failed ({reason}); {events} event(s) stay pending, see .shards/wake.log"
        )?,
        WakeOutcome::Woken {
            events,
            upto,
            confirmed,
        } => writeln!(
            out,
            "woke the core in its pane: {events} event(s) up to #{upto}{}",
            if *confirmed {
                ""
            } else {
                " (typed; not confirmed in its transcript)"
            }
        )?,
        WakeOutcome::AwaitingAck { events } => {
            if !r.fired.is_empty() {
                writeln!(
                    out,
                    "{events} event(s) pending; the core was woken, waiting for its ack"
                )?
            }
        }
        WakeOutcome::CoreDown => writeln!(
            out,
            "{} event(s) pending; the core is down, nothing typed",
            r.pending
        )?,
    }
    for n in &r.core {
        writeln!(out, "core: {n}")?;
    }
    Ok(())
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn print_status(out: &mut impl Write, s: &CoreStatus) -> io::Result<()> {
    writeln!(
        out,
        "core: {}",
        if s.started {
            "started"
        } else {
            "not started (or stopped)"
        }
    )?;
    writeln!(out, "  pane up:        {}", yes(s.pane_up))?;
    writeln!(out, "  command alive:  {}", yes(s.command_alive))?;
    writeln!(
        out,
        "  session:        {} ({})",
        s.session_id.as_deref().unwrap_or("not pinned"),
        s.adapter
    )?;
    writeln!(
        out,
        "  last wake:      {}",
        s.last_wake.as_deref().unwrap_or("never")
    )?;
    writeln!(out, "  wake mode:      {}", s.wake_mode)?;
    let wd = if !s.watchdog {
        "off".to_string()
    } else if s.gave_up {
        format!(
            "GAVE UP after {} failed restores; run mem core start",
            s.failures
        )
    } else if s.failures > 0 {
        format!("on, {} failed restore(s) in a row", s.failures)
    } else {
        "on".to_string()
    };
    writeln!(out, "  watchdog:       {wd}")?;
    writeln!(out, "  keep-warm:      {}", s.keep_warm.state)?;
    writeln!(out, "  attach:         {}", s.attach)?;
    Ok(())
}

fn fmt_n(x: f64) -> String {
    format!("{:.0}", x)
}

fn print_usage(out: &mut impl Write, entries: &[UsageEntry]) -> io::Result<()> {
    let pings = entries.iter().filter(|e| e.kind == "ping").count();
    let wakes = entries.iter().filter(|e| e.kind == "wake").count();
    let replied = entries.iter().filter(|e| e.reply).count();
    writeln!(
        out,
        "{} entries: {wakes} wake(s), {pings} ping(s), {replied} with a reply read",
        entries.len()
    )?;
    let rows = summarize_usage(entries);
    if rows.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "{:<5} {:<7} {:<8} {:>4} {:>9} {:>9} {:>12} {:>12}",
        "kind", "gap", "cache", "n", "avg in", "avg out", "avg c.read", "avg c.write"
    )?;
    for r in rows {
        writeln!(
            out,
            "{:<5} {:<7} {:<8} {:>4} {:>9} {:>9} {:>12} {:>12}",
            r.kind,
            r.gap,
            r.cache,
            r.count,
            fmt_n(r.avg_input),
            fmt_n(r.avg_output),
            fmt_n(r.avg_cache_read),
            fmt_n(r.avg_cache_creation)
        )?;
    }
    Ok(())
}

/// A shard's pinned session id, for printing.
fn session_of(meta: &ShardMeta) -> String {
    match &meta.session {
        Some(s) => s.session_id.clone().unwrap_or_else(|| "(no id)".into()),
        None => "-".into(),
    }
}

fn read_value(v: String) -> io::Result<String> {
    if v == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        Ok(s.strip_suffix('\n').map(String::from).unwrap_or(s))
    } else {
        Ok(v)
    }
}

fn print_quirk(out: &mut impl Write, q: &Quirk) -> io::Result<()> {
    writeln!(
        out,
        "{}  [{}]  {}  ({})",
        q.id,
        q.tool,
        q.quirk,
        q.state.as_str()
    )?;
    writeln!(
        out,
        "    reported by {} at {}; status: {} ({}, {})",
        q.reported_by, q.reported, q.status, q.status_by, q.status_at
    )?;
    if let Some(m) = &q.last_mentioned {
        writeln!(out, "    last mentioned {m}")?;
    }
    if let Some(a) = &q.archived_at {
        writeln!(out, "    archived {a}")?;
    }
    if let Some(d) = &q.details {
        writeln!(out, "    details: {}", d.replace('\n', "\n             "))?;
    }
    Ok(())
}

/// Quirk writes go to main even from a shard's pane, whose SHARDS_MEMORY is
/// its worktree; with no --author, the pane's SHARDS_SHARD is the author.
fn qprov(author: Option<String>) -> Provenance {
    let author = author
        .filter(|a| !a.trim().is_empty())
        .or_else(|| std::env::var("SHARDS_SHARD").ok())
        .unwrap_or_default();
    Provenance::new(author, None, "")
}

/// A missing `--why` reaches the library as empty, which refuses it with a
/// message telling the agent to say why (clap's own error doesn't).
fn prov(who: Who, why: Option<String>) -> Provenance {
    Provenance::new(
        who.author.unwrap_or_default(),
        given(who.human),
        why.unwrap_or_default(),
    )
}

/// An empty value (a shard's pane clears `SHARDS_HUMAN`) is no value.
fn given(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn read_input(file: Option<PathBuf>) -> io::Result<String> {
    match file {
        Some(p) if p.as_os_str() != "-" => std::fs::read_to_string(p),
        _ => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout().lock();
    match cli.cmd {
        Cmd::Init { dir, who } => {
            Memory::init(&dir, &prov(who, None))?;
            writeln!(out, "initialized memory at {}", dir.display())?;
        }
        Cmd::Get { address } => {
            let mem = Memory::open(&cli.memory)?;
            let f = mem.get(&Address::parse(&address)?)?;
            writeln!(out, "{}", f.value)?;
            writeln!(out, "home: {}", f.home)?;
        }
        Cmd::Set {
            address,
            value,
            why,
            answers,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let mut p = prov(who, why);
            p.answers = given(answers);
            let commit = mem.set(&a, &read_value(value)?, &p)?;
            writeln!(out, "set {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::Supersede {
            address,
            value,
            why,
            answers,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let mut p = prov(who, why);
            p.answers = given(answers);
            let commit = mem.supersede(&a, &read_value(value)?, &p)?;
            writeln!(out, "superseded {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::History { address } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            writeln!(out, "{a}  [{}]", a.home())?;
            let drafts = mem.drafts_for(a.as_str())?;
            let entries = match mem.history(&a) {
                Err(shards_core::Error::UnknownAddress(_)) if !drafts.is_empty() => {
                    writeln!(out, "\n(not on main)")?;
                    Vec::new()
                }
                r => r?,
            };
            let mut shown = std::collections::BTreeSet::new();
            for e in entries {
                writeln!(out)?;
                writeln!(out, "{}  {}", e.updated, short(&e.commit))?;
                writeln!(out, "  author: {}", e.author)?;
                writeln!(out, "  human:  {}", e.human.as_deref().unwrap_or("-"))?;
                writeln!(out, "  why:    {}", e.why)?;
                if let Some(r) = &e.review {
                    writeln!(
                        out,
                        "  review: {} accepted it from {}: {}",
                        r.by, r.proposal, r.why
                    )?;
                }
                writeln!(out, "  value:  {}", e.value.replace('\n', "\n          "))?;
                if let Some(r) = &e.review {
                    if let Some((p, c)) = drafts
                        .iter()
                        .find(|(p, c)| format!("{}#{}", p.id, c.n) == r.proposal)
                    {
                        print_drafts(&mut out, p, c)?;
                        shown.insert(r.proposal.clone());
                    }
                }
            }
            for (p, c) in &drafts {
                if !shown.contains(&format!("{}#{}", p.id, c.n)) {
                    writeln!(out)?;
                    writeln!(
                        out,
                        "{}#{} [{}], proposed by shard {}: {}",
                        p.id,
                        c.n,
                        c.status.as_str(),
                        p.shard,
                        c.value
                            .as_deref()
                            .unwrap_or("(removal)")
                            .replace('\n', " ⏎ ")
                    )?;
                    print_drafts(&mut out, p, c)?;
                }
            }
        }
        Cmd::Resolve { file, annotate } => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(file)?;
            write!(
                out,
                "{}",
                resolve(&mem, &text, ResolveOptions { annotate })?
            )?;
        }
        Cmd::GenSample { dir, seed } => {
            let s = sample::generate(&dir, seed)?;
            writeln!(
                out,
                "fake flock at {}: birds {}, {} facts, {} supersedes, notes: {}",
                dir.display(),
                s.birds.join(", "),
                s.facts,
                s.supersedes,
                s.notes.join(", ")
            )?;
        }
        Cmd::Put {
            path,
            file,
            why,
            for_person,
            answers,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(file)?;
            let person = given(for_person);
            let mut p = prov(who, why);
            p.answers = given(answers);
            let (path, commit) = mem.put_note_as(person.as_deref(), &path, &text, &p)?;
            writeln!(out, "wrote {path} ({})", short(&commit))?;
        }
        Cmd::Notes(NotesCmd::Find {
            word,
            for_person,
            json,
        }) => {
            let mem = Memory::open(&cli.memory)?;
            let hits = mem.find_notes(&word, &for_person)?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&hits)?)?;
            } else if hits.is_empty() {
                writeln!(out, "no notes mention {word:?}")?;
            }
            for h in hits.iter().filter(|_| !json) {
                let whose = match &h.owner {
                    Owner::Shared => "shared note".to_string(),
                    Owner::Person(p) if h.rank == Rank::Own => format!("{p}'s own note"),
                    Owner::Person(p) => format!("{p}'s note"),
                };
                writeln!(out, "{}  ({whose}, {})", h.path, h.day())?;
                for l in &h.lines {
                    writeln!(out, "    {l}")?;
                }
            }
        }
        Cmd::Shard(ShardCmd::Open { id, human }) => {
            let mem = Memory::open(&cli.memory)?;
            let meta = mem.shard_open(&id, given(human).as_deref())?;
            writeln!(out, "{}", meta.worktree.display())?;
            eprintln!(
                "mem: opened shard {id} on {} from main at {}",
                meta.branch,
                short(&meta.base)
            );
        }
        Cmd::Shard(ShardCmd::Start { id, human, resume }) => {
            let mem = Memory::open(&cli.memory)?;
            let human = given(human);
            match mem.shard_start(id.as_deref(), human.as_deref(), resume, ShardLaunch::Blank)? {
                ShardStart::AlreadyRunning { meta } => writeln!(
                    out,
                    "shard {} is already running (session {}); nothing to do",
                    meta.id,
                    session_of(&meta)
                )?,
                ShardStart::Started {
                    meta,
                    resumed,
                    note,
                } => {
                    writeln!(
                        out,
                        "shard {} {} in its pane; session {}",
                        meta.id,
                        if resumed { "resumed" } else { "started" },
                        session_of(&meta)
                    )?;
                    writeln!(out, "worktree: {}", meta.worktree.display())?;
                    if let Some(n) = note {
                        writeln!(out, "note: {n}")?;
                    }
                    writeln!(
                        out,
                        "attach: {}",
                        mem.shard_attach_command(&meta.id)?.join(" ")
                    )?;
                }
            }
        }
        Cmd::Shard(ShardCmd::Stop { id }) => {
            let mem = Memory::open(&cli.memory)?;
            if mem.shard_stop(&id)? {
                writeln!(
                    out,
                    "shard {id} stopped; branch, worktree and session kept (mem shard start --id {id} --resume)"
                )?;
            } else {
                writeln!(out, "shard {id} was not running")?;
            }
        }
        Cmd::Shard(ShardCmd::Attach { id, print }) => {
            use std::io::IsTerminal;
            let mem = Memory::open(&cli.memory)?;
            let cmd = mem.shard_attach_command(&id)?;
            if print || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                writeln!(out, "{}", cmd.join(" "))?;
            } else {
                drop(out);
                mem.shard_attach(&id)?;
                return Ok(());
            }
        }
        Cmd::Shard(ShardCmd::Send { id, text }) => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_value(text)?;
            match mem.shard_send(&id, &text)? {
                Landed::Confirmed => writeln!(out, "sent to {id}")?,
                Landed::Unconfirmed => writeln!(
                    out,
                    "sent to {id} (typed and entered; not yet seen in its transcript)"
                )?,
            }
        }
        Cmd::Shard(ShardCmd::List { json }) => {
            let mem = Memory::open(&cli.memory)?;
            let shards = mem.shard_list()?;
            if json {
                let rows: Vec<serde_json::Value> = shards
                    .iter()
                    .map(|s| {
                        let id = &s.meta.id;
                        serde_json::json!({
                            "id": id,
                            "human": s.meta.human,
                            "branch": s.meta.branch,
                            "opened": s.meta.opened,
                            "worktree": s.meta.worktree,
                            "pane": mem.shard_pane_state(id).as_str(),
                            "session": s.meta.session,
                            "transcript": mem.shard_transcript(id).ok().flatten(),
                            "gist": s.status.as_ref().map(|g| g.gist.clone()),
                        })
                    })
                    .collect();
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "format": 1, "shards": rows })
                    )?
                )?;
                return Ok(());
            }
            if shards.is_empty() {
                writeln!(out, "no open shards")?;
            }
            for s in shards {
                writeln!(
                    out,
                    "{}  human: {}  pane: {}  session: {}  base: {}  last commit: {}",
                    s.meta.id,
                    s.meta.human.as_deref().unwrap_or("-"),
                    mem.shard_pane_state(&s.meta.id).as_str(),
                    session_of(&s.meta),
                    short(&s.meta.base),
                    s.last_commit
                )?;
                match &s.status {
                    Some(st) => writeln!(out, "    status: {}  ({})", st.gist, st.at)?,
                    None => writeln!(out, "    status: -")?,
                }
                if !s.meta.soul_pending.is_empty() {
                    writeln!(
                        out,
                        "    soul proposal pending: {}",
                        s.meta.soul_pending.join(", ")
                    )?;
                }
            }
        }
        Cmd::Shard(ShardCmd::Status { id, gist }) => {
            let mem = Memory::open(&cli.memory)?;
            let st = mem.shard_set_status(&id, &gist)?;
            writeln!(out, "{id}: {} ({})", st.gist, st.at)?;
        }
        Cmd::Dinit { id, all_or_nothing } => {
            let mem = Memory::open(&cli.memory)?;
            let r = mem.dinit(&id, all_or_nothing)?;
            print_dinit(&mut out, &r)?;
            if r.aborted {
                return Err(format!("dinit {id} aborted: conflicts with --all-or-nothing").into());
            }
        }
        Cmd::ResolveConflict {
            target,
            take,
            why,
            shard,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let r = mem.resolve_conflict(
                &target,
                &Take::parse(&take),
                shard.as_deref(),
                &prov(who, why),
            )?;
            let i = mem.incident(&r.incident)?;
            writeln!(
                out,
                "resolved {target} (took {take}) ({})",
                short(&r.commit)
            )?;
            writeln!(out, "incident {}: {}", i.id, i.summary)?;
        }
        Cmd::Soul(SoulCmd::Approve {
            shard,
            human,
            author,
            why,
        }) => {
            let mem = Memory::open(&cli.memory)?;
            let p = Provenance::new(author.unwrap_or_default(), given(human), why);
            let commit = mem.soul_approve(&shard, &p)?;
            writeln!(
                out,
                "approved soul change from {shard} ({})",
                short(&commit)
            )?;
        }
        Cmd::Proposal(cmd) => {
            let mem = Memory::open(&cli.memory)?;
            let (d, verdict) = match cmd {
                ProposalCmd::List { all, json } => {
                    let list: Vec<Proposal> = mem
                        .proposals()?
                        .into_iter()
                        .filter(|p| all || p.is_open())
                        .collect();
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&list)?)?;
                    } else if list.is_empty() {
                        writeln!(out, "no {}proposals", if all { "" } else { "open " })?;
                    }
                    for p in list.iter().filter(|_| !json) {
                        let targets: Vec<String> = p
                            .changes
                            .iter()
                            .map(|c| format!("#{} {} [{}]", c.n, c.target(), c.status.as_str()))
                            .collect();
                        writeln!(
                            out,
                            "{}  from {} ({})  {}  {}",
                            p.id,
                            p.shard,
                            p.human.as_deref().unwrap_or("-"),
                            p.created,
                            targets.join(", ")
                        )?;
                    }
                    return Ok(());
                }
                ProposalCmd::Show { id, json } => {
                    let p = mem.proposal(&id)?;
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&p)?)?;
                    } else {
                        print_proposal(&mut out, &p)?;
                    }
                    return Ok(());
                }
                ProposalCmd::Accept { d, value } => {
                    let v = value.map(read_value).transpose()?;
                    (d, Verdict::Accept(v))
                }
                ProposalCmd::Reject { d } => (d, Verdict::Reject),
                ProposalCmd::Hold { d, ask } => (d, Verdict::Hold(ask.unwrap_or_default())),
            };
            // A shard's pane sets SHARDS_SHARD: its agent proposes, never decides.
            if let Some(s) = given(std::env::var("SHARDS_SHARD").ok()) {
                return Err(format!(
                    "shard {s} can't decide proposals; the core reviews them on main"
                )
                .into());
            }
            let p = Provenance::new(
                d.author.unwrap_or_default(),
                given(d.human),
                d.why.unwrap_or_default(),
            );
            let r = mem.proposal_decide(&d.id, d.change, &verdict, &p)?;
            for (n, target, status, record) in &r.changes {
                writeln!(
                    out,
                    "{} #{n} {target}: {}{}",
                    r.id,
                    status.as_str(),
                    record
                        .as_deref()
                        .map(|rec| format!(" (main changed it first; record {rec}, settle with mem resolve-conflict)"))
                        .unwrap_or_default()
                )?;
            }
            writeln!(out, "logged on main ({})", short(&r.commit))?;
        }
        Cmd::Incident(cmd) => {
            let mem = Memory::open(&cli.memory)?;
            match cmd {
                IncidentCmd::List { json } => {
                    let mut list = mem.incidents()?;
                    list.reverse();
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&list)?)?;
                    } else if list.is_empty() {
                        writeln!(out, "no incidents")?;
                    }
                    for i in list.iter().filter(|_| !json) {
                        writeln!(out, "{}  {}  {}", i.id, i.settled.at, i.summary)?;
                    }
                }
                IncidentCmd::Show { id, json } => {
                    let i = mem.incident(&id)?;
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&i)?)?;
                    } else {
                        print_incident(&mut out, &i)?;
                    }
                }
                IncidentCmd::Note { id, text, why, who } => {
                    let text = read_value(text)?;
                    let commit = mem.incident_note(&id, &text, &prov(who, why))?;
                    writeln!(out, "noted on {id} ({})", short(&commit))?;
                }
            }
        }
        Cmd::Question(cmd) => {
            let mem = Memory::open(&cli.memory)?;
            match cmd {
                QuestionCmd::List { all, json } => {
                    let list: Vec<_> = mem
                        .questions()?
                        .into_iter()
                        .filter(|q| all || q.is_open())
                        .collect();
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&list)?)?;
                    } else if list.is_empty() {
                        writeln!(out, "no {}questions", if all { "" } else { "open " })?;
                    }
                    for q in list.iter().filter(|_| !json) {
                        writeln!(
                            out,
                            "{}  [{}]  {}  ({}, said by {})  {}",
                            q.id,
                            q.status.as_str(),
                            q.target,
                            q.held_change(),
                            q.said_by_human.as_deref().unwrap_or(&q.said_by_shard),
                            q.ask
                        )?;
                    }
                }
                QuestionCmd::Show { id, json } => {
                    let q = mem.question(&id)?;
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&q)?)?;
                    } else {
                        print_question(&mut out, &mem, &q)?;
                    }
                }
                QuestionCmd::DontKnow { id, why, who } => {
                    let commit = mem.question_dont_know(&id, &prov(who, why))?;
                    writeln!(
                        out,
                        "logged on {id}: they don't know; it stays open for the next person ({})",
                        short(&commit)
                    )?;
                }
            }
        }
        Cmd::Shard(ShardCmd::Context { id }) => {
            let mem = Memory::open(&cli.memory)?;
            let id = given(id).or_else(|| match mem.context() {
                shards_core::Context::Shard(s) => Some(s.clone()),
                _ => None,
            });
            let human = match &id {
                Some(id) => mem.shard_meta(id).ok().and_then(|m| m.human),
                None => None,
            };
            match &id {
                Some(id) => writeln!(
                    out,
                    "You are shard {id}, with {}.\n",
                    human.as_deref().unwrap_or("no one named yet")
                )?,
                None => writeln!(out, "Not in a shard.\n")?,
            }
            write!(out, "{}", mem.questions_brief(human.as_deref())?)?;
        }
        Cmd::Board { json } => {
            let mem = Memory::open(&cli.memory)?;
            let b = mem.board(chrono::Utc::now())?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&b)?)?;
            } else {
                print_board(&mut out, &b)?;
            }
        }
        Cmd::Core(CoreCmd::Context) => {
            let mem = Memory::open(&cli.memory)?;
            let ctx = mem.core_context(chrono::Utc::now())?;
            writeln!(out, "{}", serde_json::to_string_pretty(&ctx)?)?;
        }
        Cmd::Core(CoreCmd::Start { resume }) => {
            let mem = Memory::open(&cli.memory)?;
            match mem.core_start(resume)? {
                StartReport::AlreadyRunning { session_id } => writeln!(
                    out,
                    "core already running (session {}); nothing to do",
                    session_id.as_deref().unwrap_or("not pinned")
                )?,
                StartReport::Started {
                    session_id,
                    resumed,
                } => {
                    writeln!(
                        out,
                        "core {} in its pane; session {}",
                        if resumed { "resumed" } else { "started" },
                        session_id.as_deref().unwrap_or("not pinned")
                    )?;
                    writeln!(out, "attach: {}", mem.core_attach_command()?.join(" "))?;
                }
            }
        }
        Cmd::Core(CoreCmd::Stop) => {
            let mem = Memory::open(&cli.memory)?;
            if mem.core_stop()? {
                writeln!(out, "core stopped; the watchdog will leave it stopped")?;
            } else {
                writeln!(out, "core was not running")?;
            }
        }
        Cmd::Core(CoreCmd::Status { json }) => {
            let mem = Memory::open(&cli.memory)?;
            let s = mem.core_status(chrono::Utc::now())?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&s)?)?;
            } else {
                print_status(&mut out, &s)?;
            }
        }
        Cmd::Core(CoreCmd::Attach { print }) => {
            use std::io::IsTerminal;
            let mem = Memory::open(&cli.memory)?;
            let cmd = mem.core_attach_command()?;
            if print || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                writeln!(out, "{}", cmd.join(" "))?;
            } else {
                drop(out);
                mem.core_attach()?;
                return Ok(());
            }
        }
        Cmd::Core(CoreCmd::Check) => {
            let mem = Memory::open(&cli.memory)?;
            let r = mem.core_check(chrono::Utc::now())?;
            if r.notes.is_empty() {
                writeln!(out, "core ok (or not started); nothing to do")?;
            }
            for n in r.notes {
                writeln!(out, "{n}")?;
            }
        }
        Cmd::Core(CoreCmd::Ack { upto }) => {
            let mem = Memory::open(&cli.memory)?;
            let (done, left) = mem.core_ack(upto, chrono::Utc::now())?;
            writeln!(out, "acknowledged {done} event(s); {left} still pending")?;
        }
        Cmd::Core(CoreCmd::Rinit { baton }) => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(Some(baton))?;
            let id = mem.core_rinit(&text)?;
            writeln!(
                out,
                "handed off; new session {}",
                id.as_deref().unwrap_or("(no id)")
            )?;
        }
        Cmd::Core(CoreCmd::Usage { json }) => {
            let mem = Memory::open(&cli.memory)?;
            let entries = mem.core_usage(chrono::Utc::now())?;
            if json {
                let doc = serde_json::json!({
                    "entries": entries.len(),
                    "rows": summarize_usage(&entries),
                });
                writeln!(out, "{}", serde_json::to_string_pretty(&doc)?)?;
            } else {
                print_usage(&mut out, &entries)?;
            }
        }
        Cmd::Watch { once } => loop {
            // Re-open every pass, so shards.toml changes apply at once.
            let pass = Memory::open(&cli.memory).and_then(|m| {
                let poll = m.config().awareness.poll();
                Ok((m.watch_pass(chrono::Utc::now())?, poll))
            });
            match pass {
                Ok((r, poll)) => {
                    print_pass(&mut out, &r)?;
                    out.flush()?;
                    if once {
                        break;
                    }
                    std::thread::sleep(poll);
                }
                Err(e) if once => return Err(e.into()),
                Err(e) => {
                    // Keep watching; the next pass may succeed.
                    eprintln!("mem: watch pass failed: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        },
        Cmd::Quirk(q) => {
            let mem = Memory::open(&cli.memory)?;
            match q {
                QuirkCmd::Add {
                    tool,
                    quirk,
                    by,
                    details,
                    author,
                } => {
                    let details = details.map(read_value).transpose()?;
                    let q = mem.quirk_add(
                        &NewQuirk {
                            tool,
                            quirk,
                            by,
                            details,
                        },
                        &qprov(author),
                    )?;
                    writeln!(out, "posted {} for {}: {}", q.id, q.tool, q.quirk)?;
                }
                QuirkCmd::List { tool, all, json } => {
                    let list: Vec<Quirk> = mem
                        .quirks()?
                        .into_iter()
                        .filter(|q| all || q.state == QuirkState::Open)
                        .filter(|q| tool.as_ref().is_none_or(|t| &q.tool == t))
                        .collect();
                    if json {
                        writeln!(out, "{}", serde_json::to_string_pretty(&list)?)?;
                    } else if list.is_empty() {
                        writeln!(
                            out,
                            "no {}quirks{}",
                            if all { "" } else { "open " },
                            tool.map(|t| format!(" for {t}")).unwrap_or_default()
                        )?;
                    } else {
                        for q in &list {
                            print_quirk(&mut out, q)?;
                        }
                    }
                }
                QuirkCmd::History { id } => {
                    for v in mem.quirk_history(&id)? {
                        writeln!(
                            out,
                            "{}  {}  {}  by {} (via {})  state {}: {}",
                            v.at(),
                            short(&v.commit),
                            v.op,
                            v.quirk.status_by,
                            v.author,
                            v.quirk.state.as_str(),
                            v.quirk.status
                        )?;
                    }
                }
                QuirkCmd::Update {
                    id,
                    status,
                    by,
                    author,
                } => {
                    let q = mem.quirk_update(&id, &status, &by, &qprov(author))?;
                    writeln!(out, "{} status: {} ({})", q.id, q.status, q.state.as_str())?;
                }
                QuirkCmd::Fix { id, by, author } => {
                    let q = mem.quirk_fix(&id, &by, &qprov(author))?;
                    writeln!(out, "{} status: {}", q.id, q.status)?;
                }
                QuirkCmd::Mention { id, by, author } => {
                    let q = mem.quirk_mention(&id, by.as_deref().unwrap_or(""), &qprov(author))?;
                    writeln!(
                        out,
                        "{} mentioned at {} ({})",
                        q.id,
                        q.last_mentioned.as_deref().unwrap_or("?"),
                        q.state.as_str()
                    )?;
                }
                QuirkCmd::Archive { days, by, author } => {
                    let days = days.unwrap_or(mem.config().quirks.archive_after_days);
                    let mut p = qprov(author);
                    p.human = by;
                    let done = mem.quirk_archive(days, &p)?;
                    if done.is_empty() {
                        writeln!(
                            out,
                            "nothing to archive (fixed and quiet for {days} day(s))"
                        )?;
                    }
                    for q in &done {
                        writeln!(out, "archived {}  [{}]  {}", q.id, q.tool, q.quirk)?;
                    }
                }
            }
        }
        Cmd::Debug(DebugCmd::HoldLock { secs, holder }) => {
            let mem = Memory::open(&cli.memory)?;
            let guards = mem.lock_main(&[], &holder, None)?;
            writeln!(out, "held {}", guards.len())?;
            out.flush()?;
            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            drop(guards);
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mem: {e}");
            ExitCode::FAILURE
        }
    }
}
