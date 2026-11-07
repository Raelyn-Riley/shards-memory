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
    resolve, sample, Address, CoreStatus, DinitReport, Memory, NewQuirk, PassReport, Provenance,
    Quirk, QuirkState, ResolveOptions, ShardMeta, ShardStart, Side, StartReport, Take, UsageEntry,
    WakeOutcome,
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
        why: String,
        #[command(flatten)]
        who: Who,
    },
    /// Replace the value of an existing fact (old value stays in history).
    Supersede {
        address: String,
        /// The value; `-` reads it from stdin.
        value: String,
        #[arg(long)]
        why: String,
        #[command(flatten)]
        who: Who,
    },
    /// Every version of a fact, newest first.
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
    /// needs an admin --human).
    Put {
        path: String,
        /// File to read the contents from; `-` or omitted reads stdin.
        file: Option<PathBuf>,
        #[arg(long)]
        why: String,
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
    /// Settle a conflict recorded by dinit.
    ResolveConflict {
        /// A fact address, or a repo path ending in .md (people/, notes/).
        target: String,
        /// `shard`, `main`, or a literal new value.
        #[arg(long)]
        take: String,
        #[arg(long)]
        why: String,
        /// Which shard's conflict, if several shards conflict on it.
        #[arg(long)]
        shard: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Soul proposals.
    #[command(subcommand)]
    Soul(SoulCmd),
    /// What every open shard is doing, read from git and its transcript.
    Board {
        /// Machine-readable output (format documented in the README).
        #[arg(long)]
        json: bool,
    },
    /// Evaluate [triggers] every [awareness] poll_secs; wake the core when one fires.
    Watch {
        /// Run one pass and exit (for a timer or cron).
        #[arg(long)]
        once: bool,
    },
    /// The reasoning core's view.
    #[command(subcommand)]
    Core(CoreCmd),
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
        /// Default: [quirks] archive_after_days in shards.toml (30).
        #[arg(long)]
        days: Option<f64>,
        #[arg(long, env = "SHARDS_HUMAN")]
        by: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
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

fn qprov(author: Option<String>) -> Provenance {
    Provenance::new(author.unwrap_or_default(), None, "")
}

fn prov(who: Who, why: String) -> Provenance {
    Provenance::new(who.author.unwrap_or_default(), who.human, why)
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
            Memory::init(&dir, &prov(who, String::new()))?;
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
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let commit = mem.set(&a, &read_value(value)?, &prov(who, why))?;
            writeln!(out, "set {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::Supersede {
            address,
            value,
            why,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let commit = mem.supersede(&a, &read_value(value)?, &prov(who, why))?;
            writeln!(out, "superseded {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::History { address } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            writeln!(out, "{a}  [{}]", a.home())?;
            for e in mem.history(&a)? {
                writeln!(out)?;
                writeln!(out, "{}  {}", e.updated, short(&e.commit))?;
                writeln!(out, "  author: {}", e.author)?;
                writeln!(out, "  human:  {}", e.human.as_deref().unwrap_or("-"))?;
                writeln!(out, "  why:    {}", e.why)?;
                writeln!(out, "  value:  {}", e.value.replace('\n', "\n          "))?;
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
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(file)?;
            let commit = mem.put_file(&path, &text, &prov(who, why))?;
            writeln!(out, "wrote {path} ({})", short(&commit))?;
        }
        Cmd::Shard(ShardCmd::Open { id, human }) => {
            let mem = Memory::open(&cli.memory)?;
            let meta = mem.shard_open(&id, human.as_deref())?;
            writeln!(out, "{}", meta.worktree.display())?;
            eprintln!(
                "mem: opened shard {id} on {} from main at {}",
                meta.branch,
                short(&meta.base)
            );
        }
        Cmd::Shard(ShardCmd::Start { id, human, resume }) => {
            let mem = Memory::open(&cli.memory)?;
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
            let commit = mem.resolve_conflict(
                &target,
                &Take::parse(&take),
                shard.as_deref(),
                &prov(who, why),
            )?;
            writeln!(out, "resolved {target} (took {take}) ({})", short(&commit))?;
        }
        Cmd::Soul(SoulCmd::Approve {
            shard,
            human,
            author,
            why,
        }) => {
            let mem = Memory::open(&cli.memory)?;
            let p = Provenance::new(author.unwrap_or_default(), human, why);
            let commit = mem.soul_approve(&shard, &p)?;
            writeln!(
                out,
                "approved soul change from {shard} ({})",
                short(&commit)
            )?;
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
