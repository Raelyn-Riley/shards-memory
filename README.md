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
recorded for a human, never guessed. A soul change waits for a human admin. Built so
far: facts, supersede, history, pointers, shards, dinit, conflicts, soul approval,
locks with lease-based crash recovery, and a fake-data generator. Bulletins, the drift
checker and the web UI come later. See [`docs/design.md`](docs/design.md) for the
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
```

Local state that is never committed lives in `.shards/`: `worktrees/`, `shards/`
(shard records), `status/`, `locks/` and `lock.log`.

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
