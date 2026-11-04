# shards

`shards` is a shared memory for AI agents. A *memory* is a plain git repository of
markdown fact files. Every fact lives at exactly one path, and that path is its only
home: `facts/devices/rotoforge-ctl/ip.md` is addressed everywhere as
`devices/rotoforge-ctl/ip`. Notes and chat use pointers such as
`{{fact:devices/rotoforge-ctl/ip}}` in place of copied values, so a fact changes in one
place and every reader sees the new value. Each change is one commit on `main`, stamped
with the time, the shard (session) that made it, the human at the keyboard and the
reason. Old values stay readable through history, and nothing in the tool can rewrite
that history.

All reads and writes go through one small tool, `mem`, which is a thin CLI over the
`shards-core` library. Each session works in a *shard*: its own branch and git
worktree. When the session is done the shard *dinits*, and its work is merged into
`main` under a lock. If a shard and main both changed the same fact, the conflict is
recorded for a human, never guessed. A soul change waits for a human admin. The core
knows what every shard is doing without asking: `mem board` reads it from git and from
each shard's agent transcript, and `mem watch` wakes a reasoning core only when a
configured trigger fires. Built so far: facts, supersede, history, pointers, shards,
dinit, conflicts, soul approval, locks with lease-based crash recovery, the board, the
watcher and the wake command, and a fake-data generator. Bulletins, the drift checker
and the web UI come later. See [`docs/design.md`](docs/design.md) for the
design. Licensed AGPL-3.0-or-later.

## Build

Requires a stable Rust toolchain (via [rustup](https://rustup.rs)) and `git` ≥ 2.28 on
`PATH`.

```sh
cargo build --release     # binary at target/release/mem
cargo test
```

### Why shell out to `git` instead of libgit2

`shards-core` runs the `git` binary through a small wrapper (`crates/shards-core/src/git.rs`)
instead of linking `git2`/libgit2. The reasons:

- **The guarantee is easy to check.** The wrapper runs only an allowlist of subcommands
  (`init add commit log show cat-file rev-parse rev-list symbolic-ref status diff-tree
  ls-files ls-tree merge-base worktree hash-object read-tree update-index write-tree
  commit-tree merge branch`). It refuses `--amend --force -f --fixup --squash -D -M -C
  --hard` on any of them. `merge` must be `--ff-only`, `branch` may only `-d` (delete a
  branch that is already merged), and `worktree` may only `add`, `remove` or `list`.
  Nothing can call `rebase`, `reset`, `push`, `update-ref` or `commit --amend`, and a
  unit test asserts this.
- **It behaves exactly like git.** Users and agents inspect the memory with the same
  `git log` and `git show` that the tool uses.
- **No C dependency.** That means no libgit2, OpenSSL or pkg-config build chain.

The cost is one process per git call. That is fine at this scale.

## Commands

Global: `-C, --memory <dir>` (or `SHARDS_MEMORY`) picks the memory and defaults to
`.`. Writes need an author from `--author` or `SHARDS_AUTHOR`. A human can be given with
`--human` or `SHARDS_HUMAN`. A value of `-` reads the value from stdin.

| Command | Example |
| --- | --- |
| `mem init <dir>` creates a new memory (`facts/`, `soul/`, `people/`, README, `.gitignore`, a default `shards.toml`, first commit). | `mem init ~/flock-memory` |
| `mem get <address>` prints the value, then `home: <path>`. An unknown address exits 1. | `mem get devices/rotoforge-ctl/ip` |
| `mem set <address> <value> --why ...` creates a **new** fact. It refuses only an exact existing address (use `supersede`). Near-duplicates such as the same name elsewhere are left to the later drift checker, which flags them for a human. | `mem set devices/rotoforge-ctl/ip 10.0.4.40 --why "first seen on the bench"` |
| `mem supersede <address> <value> --why ...` replaces an existing fact's value as one commit that touches only that file. | `mem supersede devices/rotoforge-ctl/ip 10.0.4.41 --why "moved to the new switch"` |
| `mem history <address>` shows every version, newest first: time, commit, author, human, why and value. | `mem history devices/rotoforge-ctl/ip` |
| `mem resolve [file\|-] [--annotate]` replaces every `{{fact:...}}` with the current value. `--annotate` gives `10.0.4.41 [devices/rotoforge-ctl/ip]`. Any unresolvable pointer is an error that names it. | `mem resolve notes/runbook.md --annotate` |
| `mem gen-sample <dir> --seed N` builds a fake flock: 3 birds, 30 facts in 4 sections, 3 supersedes, 3 notes with pointers. The same seed gives byte-identical commits. | `mem gen-sample /tmp/flock --seed 7` |
| `mem put <path> [file\|-] --why ...` writes a `notes/`, `people/` or `soul/` markdown file as one commit. Writing `soul/` on main needs an admin `--human`. | `mem put people/ada.md profile.md --why "prefers short answers"` |
| `mem shard open --id <id> [--human <name>]` creates branch `shard/<id>` from main in its own worktree and prints the worktree path. | `cd "$(mem shard open --id s7 --human Ada)"` |
| `mem shard list` lists open shards: human, base commit, last commit time and status line. | `mem shard list` |
| `mem shard status --id <id> "<gist>"` sets the shard's one-line status. | `mem shard status --id s7 "moving the controller"` |
| `mem dinit --id <id> [--all-or-nothing]` merges the shard into main under the lock and reports what was merged, conflicted, proposed or skipped. | `mem dinit --id s7` |
| `mem resolve-conflict <address\|path.md> --take shard\|main\|<value> --why ... [--shard <id>]` settles a recorded conflict on main. | `mem resolve-conflict devices/ctl/ip --take shard --why "s7's switch is live"` |
| `mem soul approve <shard-id> --human <admin>` writes a shard's pending soul proposal to main. Admins only. | `mem soul approve s7 --human Ada` |
| `mem board [--json]` shows every open shard (human, state, gist, last activity, facts in flight, last few actions), then overlaps, open conflicts and soul proposals. Takes no lock. | `mem board --json` |
| `mem watch [--once]` evaluates the triggers every `poll_secs`, records events and wakes the core. `--once` runs one pass and exits. | `mem watch --once` |
| `mem core context` prints `mem board --json` and the pending events as one JSON document: everything a woken core needs. | `mem core context` |

## Shards

`mem shard open --id s7` creates branch `shard/s7` from the current `main` in a separate
git worktree. The default location is `.shards/worktrees/s7` inside the memory, which is
gitignored; `[shards] worktree_dir` changes it. The shard's base commit, human and
worktree are recorded in `.shards/shards/s7.json`.

Inside the worktree, `get`, `set`, `supersede`, `history`, `resolve` and `put` work
exactly as on main, but they commit to `shard/s7`:

- The author defaults to the shard id.
- Writing as any other author is refused, because a worktree belongs to its shard.
- Writes are allowed only on `main` or on your own `shard/<id>` branch.

The status line lives in `.shards/status/<id>.json`, outside git, so it can never cause
a merge conflict.

## Dinit

`mem dinit --id s7` merges `shard/s7` into main while holding the shard's lock and the
main lock(s). It looks at every file changed on the shard since its base
(`git merge-base main shard/s7`) and handles each one by where it lives:

| File | What happens |
| --- | --- |
| `facts/`, `people/`, `notes/`, changed only on the shard | Applied to main. |
| `facts/`, `people/`, `notes/`, changed on the shard **and** on main since the base | **Conflict.** Main's version stays. A record goes to `conflicts/s7/<address>.md` (non-fact files: `conflicts/s7/@/<path>`) with both values, authors, humans, times and whys. The other files still merge unless `--all-or-nothing` is given, in which case nothing is written. |
| `facts/`, `people/`, `notes/`, identical on both sides | Nothing to do. |
| `soul/` | Never merged automatically. Reported as a **proposal** that waits for `mem soul approve`. |
| Anything else (`shards.toml`, `conflicts/`, README, ...) | Skipped and reported. |

**How it merges.** Dinit builds the merge tree itself, in a temporary index, from the
rules above. It writes a real two-parent merge commit (`main` + the shard tip) with
`commit-tree`, then moves main to it with `git merge --ff-only`. There is no rebase, no
replay and no rewriting. Git's own content merge is never used. Because the shard's
commits become ancestors of main:

- `mem history` on main shows every shard version with its own author, time and why.
- The next dinit's base is simply the new merge-base, so after a partial dinit a shard
  can keep working and dinit again. Settled facts aren't re-reported, and new changes
  to a conflicted fact conflict again.

**When the shard closes.** A dinit is clean when it has no conflicts, no pending soul
proposal and no open records under `conflicts/<id>/`. A clean dinit removes the shard's
worktree (one with uncommitted changes is kept and reported). It keeps or deletes the
branch per `[shards] keep_branches`, then drops the shard from `mem shard list`.
Otherwise the shard stays open until its conflicts and proposals are settled.

### Conflicts

`mem resolve-conflict devices/ctl/ip --take shard|main|<value> --why "..."` settles a
conflict on main as one commit, which also deletes the conflict record.

- `main` keeps main's value.
- `shard` writes the shard's value.
- Anything else is taken as a literal new value. A literal value that is exactly `shard`
  or `main` can't be given this way.
- The fact's new front matter records the resolver and the `--why`. The commit adds
  `Shards-Take` and `Shards-Shard` trailers.
- If several shards conflict on the same fact, pick one with `--shard <id>`.
- Profiles and notes are named by path: `mem resolve-conflict people/ada.md --take shard ...`.

### Soul approval

A soul change made on a shard is only a proposal. `mem soul approve s7 --human Ada`
checks that `Ada` is listed in `[soul] admins` in `shards.toml`. If so, it writes the
shard's proposed `soul/` files to main as one commit with a `Shards-Approved-By`
trailer. Otherwise it refuses and writes nothing. On main itself, `mem put soul/...`
also needs an admin `--human`.

## Locks

Only one writer changes main at a time:

- `set`, `supersede`, `put` and `resolve-conflict` on main take the main lock(s), and so
  do `soul approve` and every `dinit`.
- Each shard's own writes take a per-shard lock (`shard.<id>`). Dinit takes the shard
  lock first, then the main lock(s), so a shard can't commit mid-dinit.
- With `scope = "section"`, the main lock is per top-level section: `main.facts.<section>`,
  `main.people`, `main.soul`, `main.notes` and `main.conflicts.<shard>`. A dinit takes the
  locks of every section it touches, in sorted order, so two dinits can't deadlock.

**The lock file.** Each lock is `.shards/locks/<name>.lock`, holding JSON: holder id,
human, pid, host, acquired time, lease expiry and a nonce.

- **Atomic acquire.** Every acquire, renew, release and break happens while holding
  `flock(2)` on a sibling `<name>.guard`. That guard is held for microseconds and freed
  by the kernel if the process dies. Under it, the lock file is created with `O_EXCL`.
  The file itself is the lock, and it deliberately outlives a crashed holder, so that
  recovery is explicit and logged rather than silent. Because the guard serializes
  "check if stale, break it, take it", two waiters can never both break the same stale
  lock.
- **Blocked writers.** A blocked writer is told `locked by <holder> (<human>) since
  <time>`. In wait mode it then polls with backoff until `timeout_secs`. It is not a
  strict FIFO queue. In fail mode it exits non-zero at once.
- **Heartbeat and lease.** A holder's background thread renews the lease every
  `lease_secs / 3`.
- **Recovery.** A lock is recovered only if its lease has expired **and** its pid is gone
  on this host (a zombie counts as gone). A lock from another host is never broken
  automatically. Each recovery is appended to `.shards/lock.log` and told to the writer
  that recovered it.
- **The git mutex.** A short internal `flock` on `.shards/locks/git.mutex` wraps every
  index/HEAD update. With section scope, two writers can hold different section locks
  at once, but main has one HEAD, and this keeps their commits from racing.

Test hook: `SHARDS_DEBUG_HOLD_MS=<n>` makes a writer sleep for `n` ms after taking its
locks. The tests use it to force contention, and `mem debug hold-lock` (hidden) holds
the main lock for the crash test.

## Awareness

The core must know what every shard is doing, without the shard having to report it.
A shard can forget, be wrong, or waste effort on status. So awareness is *read*, not
reported. Shards just keep working, and three layers watch them:

1. **Observe.** For each open shard, read what it is actually doing: its branch and its
   agent's session transcript.
2. **Data core.** `mem board` and `mem watch` are plain code. They never call a model,
   so they are always cheap to run.
3. **Reasoning core.** An external command of your choice, run only when a configured
   trigger fires.

### Observe

Each open shard is read from two places:

- **Git.** Every `facts/`, `people/` and `notes/` path changed on `shard/<id>` since its
  merge-base with main, plus uncommitted changes in its worktree. Together these are
  the shard's work *in flight*.
- **The activity source.** When the shard last did anything, and its recent actions.

The gist from `mem shard status` is still shown, but nothing depends on it.

**Last activity** is the latest of: the newest transcript record, the newest commit on
the shard that main doesn't have, the newest mtime of an uncommitted file, and the time
the shard was opened. Its age gives the **state**: `active` below `idle_secs`, `idle`
below `stale_secs`, and `stale` beyond that.

**Activity sources.** `ActivitySource` is a trait in `shards-core`, so shards isn't tied
to one agent tool. Pick one with `[activity] source`:

- `claude-code` (the default) reads Claude Code session transcripts.
- `none` reads git only, with no transcript.

A missing or unreadable transcript never causes an error. The board just falls back to
git.

**Where Claude Code keeps transcripts.** Claude Code writes one JSONL file per session
to `~/.claude/projects/<slug>/<session-id>.jsonl`. If `CLAUDE_CONFIG_DIR` is set, the
directory is `$CLAUDE_CONFIG_DIR/projects` instead, and `[activity] transcript_dir`
overrides both. The slug is the session's working directory with *every* character
other than `A-Z a-z 0-9` replaced by `-`, not just `/`. For example,
`/m/.shards/worktrees/s1.v2` becomes `-m--shards-worktrees-s1-v2`. A slug longer than
200 characters is cut to 200 and gets a hash appended, so shards matches those
directories by prefix. A shard's worktree is its session's working directory, so its
transcripts can be found from the worktree path alone. All `*.jsonl` files in that
directory are read, one per session.

**Privacy (hard rule).** Transcripts hold prompts, answers, file contents and tool
results. None of that is ever stored or printed. From each transcript record shards
keeps only:

- the record's time, and
- for each tool call, its tool name plus either a file **basename** or, for a shell
  command, at most **80 characters** of the command.

Commands are redacted before they are cut, so a cut can't leave half a secret behind.
Redaction removes:

- bearer, basic and token auth values
- `password=`, `token:`, `API_KEY=` and similar assignments
- `--password x` style flags
- JWTs
- hex strings of 32 characters or more
- well-known token prefixes (`ghp_`, `sk-`, `xox?-`, `AKIA...`)
- `user:pass@` in URLs

Transcripts are read incrementally. The state keeps a byte offset per file, reads only
complete new lines, and leaves a half-written last line for the next read. All of this
is stored in `.shards/activity/<id>.json`. The tests plant a secret and prompt text in
a fake transcript and check that neither shows up in any output or state file.

### The board: `mem board`

```text
s1  Ada  active  last activity 2m ago (transcript)  in flight: 1 fact(s)
    gist:   moving the kiln controller
    flight: devices/kiln-east/ip  (* uncommitted)
     3m ago  Edit  ip.md
     2m ago  Bash  curl -H "Authorization: Bearer [REDACTED]" https://kiln.example.invalid/…
s2  Basil  idle  last activity 14m ago (worktree)  in flight: 1 fact(s)
    flight: devices/kiln-east/ip*  (* uncommitted)

overlaps:
  same-fact     devices/kiln-east/ip  shards s1, s2 (Ada, Basil)
  same-section  devices  shards s1, s2 (Ada, Basil)
```

**Overlaps** are computed in code from what is in flight:

- **`same-fact`**: two or more open shards have the same fact (or the same `people/` or
  `notes/` file) in flight. This is a conflict caught before dinit.
- **`same-section`**: two or more open shards are in flight in the same top-level
  section (a fact's first address segment, or `people` or `notes`).

Open conflict records on main and pending soul proposals are listed under the overlaps.

Reading the board never takes the main lock or a shard lock, so it never blocks a
writer. It runs only read-only git commands, with `GIT_OPTIONAL_LOCKS=0` so that
`git status` doesn't take the index lock. It also takes a per-shard `flock` on
`.shards/activity/<id>.guard` while it updates that shard's activity file.

**`mem board --json`** is a stable format. `format` changes only on an incompatible
change.

```jsonc
{
  "format": 1,
  "generated": "2026-11-04T22:52:48Z",        // RFC 3339 UTC
  "memory": "/srv/flock-memory",               // main worktree
  "activity_source": "claude-code",            // or "none"
  "thresholds": { "idle_secs": 600.0, "stale_secs": 3600.0 },
  "shards": [{
    "id": "s1", "human": "Ada",                // human may be null
    "branch": "shard/s1",
    "state": "active",                         // "active" | "idle" | "stale"
    "gist": "moving the kiln controller",      // null if never set
    "gist_at": "2026-11-04T22:40:00Z",         // null if never set
    "opened": "2026-11-04T22:30:00Z",
    "last_activity": "2026-11-04T22:50:12Z",
    "age_secs": 156,
    "last_activity_from": "transcript",        // "transcript" | "commit" | "worktree" | "opened"
    "in_flight": [{
      "path": "facts/devices/kiln-east/ip.md",
      "address": "devices/kiln-east/ip",       // null for people/ and notes/
      "section": "devices",                    // or "people" / "notes"
      "uncommitted": false                     // true if changed in the worktree, not committed
    }],
    "facts_in_flight": 1,
    "recent_actions": [                        // at most 5, newest last
      { "at": "2026-11-04T22:49:00.000Z", "tool": "Edit", "summary": "ip.md" }
    ],
    "soul_pending": []
  }],
  "overlaps": [{
    "kind": "same-fact",                       // or "same-section"
    "target": "devices/kiln-east/ip",          // address/path, or the section
    "shards": ["s1", "s2"], "humans": ["Ada", "Basil"],
    "addresses": ["devices/kiln-east/ip"]
  }],
  "conflicts": [{ "shard": "s3", "target": "network/dns",
                  "record": "conflicts/s3/network/dns.md", "humans": ["Ada", "Cleo"] }],
  "soul_proposals": [{ "shard": "s4", "human": "Dov", "paths": ["soul/voice.md"] }]
}
```

### The watcher: `mem watch`

`mem watch` runs a pass every `[awareness] poll_secs`. It re-reads `shards.toml` before
each pass. `mem watch --once` runs one pass and exits, so a systemd timer or cron can
drive it. The watcher never calls a model itself. If a pass fails, the error is printed
and the watcher keeps going.

A pass builds the board and checks the **triggers** that are switched on:

| Trigger | Fires when | Dedupe key |
| --- | --- | --- |
| `same_fact` | a `same-fact` overlap appears | `same-fact:<address>:<shards>` |
| `same_section` | a `same-section` overlap appears | `same-section:<section>:<shards>` |
| `stale_with_work` | a shard is `stale` with at least one fact in flight | `stale-with-work:<id>` |
| `open_conflict` | a conflict record exists on main | `open-conflict:<record path>` |
| `soul_proposal` | a shard has a pending soul proposal | `soul-proposal:<id>:<paths>` |

When a condition starts to hold, one **event** is appended to `.shards/events.jsonl`
and added to the pending list:

```json
{"id":1,"time":"2026-11-04T22:52:48.237Z","kind":"same-fact","shards":["s1","s2"],
 "addresses":["devices/kiln-east/ip"],"humans":["Ada","Basil"],
 "key":"same-fact:devices/kiln-east/ip:s1,s2",
 "summary":"shards s1, s2 both have devices/kiln-east/ip in flight"}
```

**Dedupe.** A condition fires once while it holds. When it stops holding it is
forgotten, so if it comes back it fires again. A third shard joining an overlap changes
the key, so that counts as a new condition.

**Restarts.** The state lives in `.shards/watch.json`. It holds the conditions that held
at the last pass, the pending events, the last wake time and the next event id. It is
written after every pass and read by the next one, whichever process runs it. A
restarted `mem watch`, or the next `mem watch --once`, therefore carries on where the
last pass stopped:

- Conditions that still hold don't fire again.
- Pending events are still pending.
- The cooldown still counts from the last wake.

A condition that cleared and came back while no watcher was running looks as if it held
all along, so it doesn't fire again. If `watch.json` is unreadable, it is moved to
`watch.json.bad` and the problem is logged in `wake.log`. The watcher then starts fresh,
so every current condition fires once.

### Waking the core

If `[wake] command` is set, a pass may wake the core. It does so when events are
pending and the last wake was at least `cooldown_secs` ago. The pass then runs the
command with `sh -c` in the memory root, and gives it:

- the **pending events as a JSON array on stdin**, and
- `SHARDS_MEMORY` set to the memory root in its environment.

The command's stdout and stderr go to the watcher's. The command may run for up to
`timeout_secs`. After that it is killed, along with its whole process group.

- **Success** (exit 0): those events are delivered and leave the pending list.
- **Failure or timeout**: a line goes to `.shards/wake.log`, the events stay pending,
  and the watcher carries on. Successful wakes are logged too.
- **Cooldown**: counts from every attempt, including failed ones, because a failed run
  may still have cost money. Events that fire during a cooldown wait for the next wake
  and are all handed over together.
- **No command** (the default): events are recorded and stay pending, but nothing runs.
  The pending list keeps the newest 1000 events. Older ones remain in `events.jsonl`.
- **One at a time**: only one wake runs at a time, even across several `mem watch`
  processes. It is guarded by an `flock` on `.shards/wake.lock`. During a wake, other
  passes keep recording events.

A woken core reads one thing: `mem core context`. That is `{"format":1, "board":
<mem board --json>, "events": [<pending events>]}`. The command is whatever the operator
chooses. For example (not run by anything in this repo, and never in tests):

```toml
[wake]
# command = """
# claude -p "You are the shards core. Events are on stdin. Run 'mem core context' for
# the board, then decide which humans should talk to each other." \
#   --allowedTools "Bash(mem core context)" >> "$SHARDS_MEMORY/.shards/core.log"
# """
timeout_secs = 300
cooldown_secs = 600
```

## Config: `shards.toml`

`shards.toml` sits at the memory root. `mem init` writes the defaults, and every key is
optional. Unknown keys are an error, so a typo doesn't silently fall back to a default.
The file is read fresh by every `mem` run, so a change takes effect without any code
edit.

```toml
[lock]
scope = "section"     # "store": one lock for main | "section": one per top-level section
when_busy = "wait"    # "wait": queue up to timeout_secs | "fail": fail fast
timeout_secs = 30     # how long a waiting writer waits
lease_secs = 30       # lock lifetime without a heartbeat; a dead holder's lock is recovered after this

[soul]
admins = ["Ada", "Basil"]   # humans who may approve soul changes

[shards]
keep_branches = true               # keep shard/<id> after a clean dinit (false: branch -d)
worktree_dir = ".shards/worktrees" # relative to the memory root, or absolute

[activity]
source = "claude-code"  # "claude-code": read session transcripts | "none": git only
transcript_dir = ""     # empty: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects

[awareness]
idle_secs = 600         # no activity this long: idle
stale_secs = 3600       # no activity this long: stale
poll_secs = 30          # how often mem watch runs a pass

[triggers]              # each on or off
same_fact = true
same_section = false
stale_with_work = true  # stale shard with facts in flight
open_conflict = true
soul_proposal = true

[wake]
command = ""            # empty: record events, run nothing
timeout_secs = 300
cooldown_secs = 600     # wake the core at most once per this long
```

Local state that is never committed lives in `.shards/`: `worktrees/`, `shards/`
(shard records), `status/`, `locks/`, `lock.log`, `activity/` (per-shard activity
summaries and transcript offsets), `events.jsonl`, `watch.json` (watcher state) and
`wake.log`.

Address rules: at least `section/name`. Segments use only `a-z 0-9 - _ .`, start with a
letter or digit, and carry no `facts/` prefix or `.md` suffix. Lowercase only, so two
homes can't differ just by case.

## Fact file format

```markdown
---
address: "devices/rotoforge-ctl/ip"
updated: "2026-11-03T15:04:05Z"
author: "shard-7f3a"
human: "Tess"
why: "moved to the new switch"
---
10.0.4.41
```

- The file opens with YAML front matter. Keys come in the fixed order above, and each
  value is a double-quoted string with JSON escaping, which is also valid YAML.
- `updated` is RFC 3339 UTC, written by the tool. It is never edited by hand.
- `human` is omitted when no human is known. `author`, `human` and `why` are single-line.
- The body is the value followed by exactly one newline. Values may span several lines.
- Unknown front-matter keys are ignored when reading.

## Commit message format

```text
supersede: devices/rotoforge-ctl/ip

Shards-Op: supersede
Shards-Address: devices/rotoforge-ctl/ip
Shards-Author: shard-7f3a
Shards-Human: Tess
Shards-Why: moved to the new switch
```

- The subject is `<op>: <subject>`. The ops and their subjects are:
  - `init: memory`
  - `set` and `supersede`: the address, plus a `Shards-Address` trailer.
  - `put`: the path, plus `Shards-Path`.
  - `dinit`: the shard id. This is the merge commit, with one line of counts and the
    trailers `Shards-Shard`, `Shards-Merged`, `Shards-Conflict` and `Shards-Proposal`
    (one per path).
  - `resolve-conflict`: the target, plus `Shards-Target`, `Shards-Shard` and
    `Shards-Take`.
  - `soul-approve`: the shard id, plus `Shards-Shard`, `Shards-Approved-By` and
    `Shards-From-Commit`.
- The body is git trailers, so `git log --format='%(trailers:key=Shards-Why,valueonly)'`
  works. `Shards-Human` is omitted when there is no human.
- The git author and committer are both the shard: name `<author>`, email
  `<author>@shards.invalid`. Both git dates equal the fact's `updated` time.
- Every write commits exactly the paths it changes (`git commit --only -- <paths>`):
  one file for `set`, `supersede` and `put`. Other changes staged in the repo stay out.
  Writes are refused unless the worktree is on `main` or on your own shard branch, and
  refused if a target file has uncommitted edits.
