// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Write locks for main (and for each shard's branch).
//!
//! # How a lock works
//!
//! A lock is a file `.shards/locks/<name>.lock` holding JSON: holder id,
//! human, pid, host, acquired time, lease expiry, and a random nonce. The lock
//! *is* the file existing; it outlives a crashed process on purpose, so that
//! recovery is explicit, lease-based and logged.
//!
//! Every read-check-write of a lock file (acquire, renew, release, break) is
//! done while holding `flock(2)` on a sibling `<name>.guard` file (via
//! `std::fs::File::lock`). The guard is held for microseconds and is released
//! by the kernel if the process dies, so it can never wedge. Under the guard,
//! acquire creates the lock file with `O_EXCL` (`create_new`) as a second line
//! of defence. Taken together this makes "is it stale? then break it and take
//! it" a single atomic step: two waiters can never both break the same stale
//! lock, and a reader never sees a half-written file.
//!
//! A holder runs a heartbeat thread that pushes `lease_expiry` forward every
//! lease/3. A lock is recovered only if its lease has expired AND its pid is
//! gone on this host (a zombie counts as gone). A lock from another host is
//! never broken automatically. Each recovery is appended to
//! `.shards/lock.log` and reported to the blocked writer.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{LockConfig, WhenBusy};
use crate::error::{Error, Result};

/// What is written in a lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub name: String,
    pub holder: String,
    pub human: Option<String>,
    pub pid: u32,
    pub host: String,
    /// RFC 3339.
    pub acquired: String,
    /// RFC 3339.
    pub lease_expiry: String,
    pub nonce: String,
}

impl LockInfo {
    /// "locked by <holder> (<human>) since <time>"
    pub fn describe(&self) -> String {
        format!(
            "locked by {} ({}) since {}",
            self.holder,
            self.human.as_deref().unwrap_or("-"),
            self.acquired
        )
    }

    fn expired(&self, now: DateTime<Utc>) -> bool {
        DateTime::parse_from_rfc3339(&self.lease_expiry)
            .map(|t| t.with_timezone(&Utc) <= now)
            .unwrap_or(true)
    }
}

/// Called with a human-readable line when a writer is blocked or a lock is
/// recovered.
pub type Notifier = Arc<dyn Fn(&str) + Send + Sync>;

pub fn stderr_notifier() -> Notifier {
    Arc::new(|msg: &str| eprintln!("mem: {msg}"))
}

/// Hands out locks in one lock directory.
#[derive(Clone)]
pub struct Locks {
    dir: PathBuf,
    log: PathBuf,
    cfg: LockConfig,
    notify: Notifier,
}

impl std::fmt::Debug for Locks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locks").field("dir", &self.dir).finish()
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn nonce() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", std::process::id(), t)
}

/// This machine's hostname.
pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// Is `pid` a live (non-zombie) process on this host?
pub fn pid_alive(pid: u32) -> bool {
    let proc_dir = Path::new("/proc");
    if proc_dir.join("self").exists() {
        match fs::read_to_string(proc_dir.join(pid.to_string()).join("stat")) {
            // Field 3 (after the parenthesised command name) is the state.
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X"),
            Err(_) => false,
        }
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(true)
    }
}

enum Attempt {
    Acquired(LockInfo),
    Busy(LockInfo),
}

impl Locks {
    pub fn new(
        dir: impl Into<PathBuf>,
        log: impl Into<PathBuf>,
        cfg: LockConfig,
        notify: Notifier,
    ) -> Self {
        Locks {
            dir: dir.into(),
            log: log.into(),
            cfg,
            notify,
        }
    }

    pub fn config(&self) -> &LockConfig {
        &self.cfg
    }

    fn lock_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.lock"))
    }

    /// Run `f` while holding the guard flock for `name`.
    fn guarded<T>(&self, name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.dir)?;
        let guard = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(format!("{name}.guard")))?;
        guard.lock()?;
        let out = f();
        let _ = guard.unlock();
        out
    }

    /// Read a lock file. A file that exists but can't be parsed (a writer
    /// died mid-write) is reported as an anonymous, already-expired lock from
    /// a dead pid on this host, once it is older than the lease.
    fn read(&self, name: &str) -> Result<Option<LockInfo>> {
        let path = self.lock_path(name);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if let Ok(info) = serde_json::from_str::<LockInfo>(&text) {
            return Ok(Some(info));
        }
        let modified: DateTime<Utc> = fs::metadata(&path)?.modified()?.into();
        let lease = chrono::Duration::from_std(self.cfg.lease()).unwrap_or_default();
        Ok(Some(LockInfo {
            name: name.to_string(),
            holder: "(unreadable lock file)".to_string(),
            human: None,
            pid: 0,
            host: hostname(),
            acquired: rfc3339(modified),
            lease_expiry: rfc3339(modified + lease),
            nonce: String::new(),
        }))
    }

    fn write_new(&self, info: &LockInfo) -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.lock_path(&info.name))?;
        f.write_all(
            serde_json::to_string_pretty(info)
                .expect("serializable")
                .as_bytes(),
        )?;
        f.sync_all()?;
        Ok(())
    }

    fn log(&self, line: &str) {
        if let Some(parent) = self.log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = writeln!(f, "{line}");
        }
    }

    fn try_once(&self, name: &str, holder: &str, human: Option<&str>) -> Result<Attempt> {
        self.guarded(name, || {
            let t = now();
            if let Some(cur) = self.read(name)? {
                let same_host = cur.host == hostname();
                let recoverable = cur.expired(t) && same_host && !pid_alive(cur.pid);
                if !recoverable {
                    return Ok(Attempt::Busy(cur));
                }
                fs::remove_file(self.lock_path(name))?;
                let line = format!(
                    "{} recovered lock `{name}` from {} ({}) pid {} on {}: lease expired {}, process gone; taken by {holder} pid {}",
                    rfc3339(t),
                    cur.holder,
                    cur.human.as_deref().unwrap_or("-"),
                    cur.pid,
                    cur.host,
                    cur.lease_expiry,
                    std::process::id()
                );
                self.log(&line);
                (self.notify)(&line);
            }
            let lease = chrono::Duration::from_std(self.cfg.lease()).unwrap_or_default();
            let info = LockInfo {
                name: name.to_string(),
                holder: holder.to_string(),
                human: human.map(String::from),
                pid: std::process::id(),
                host: hostname(),
                acquired: rfc3339(t),
                lease_expiry: rfc3339(t + lease),
                nonce: nonce(),
            };
            match self.write_new(&info) {
                Ok(()) => Ok(Attempt::Acquired(info)),
                // Someone created it without the guard; treat as busy.
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(Attempt::Busy(self.read(name)?.unwrap_or(info)))
                }
                Err(e) => Err(e),
            }
        })
    }

    /// Acquire one lock, honouring `when_busy`, `timeout_secs` and the lease.
    pub fn acquire(&self, name: &str, holder: &str, human: Option<&str>) -> Result<LockGuard> {
        let start = Instant::now();
        let mut told: Option<String> = None;
        let mut pause = Duration::from_millis(20);
        loop {
            match self.try_once(name, holder, human)? {
                Attempt::Acquired(info) => return Ok(LockGuard::start(self.clone(), info)),
                Attempt::Busy(cur) => {
                    let msg = cur.describe();
                    if self.cfg.when_busy == WhenBusy::Fail {
                        return Err(Error::Locked {
                            name: name.into(),
                            msg,
                        });
                    }
                    if start.elapsed() >= self.cfg.timeout() {
                        return Err(Error::LockTimeout {
                            name: name.into(),
                            msg,
                            waited: start.elapsed().as_secs_f64(),
                        });
                    }
                    if told.as_deref() != Some(cur.nonce.as_str()) {
                        (self.notify)(&format!(
                            "{msg}; waiting for lock `{name}` (up to {}s)",
                            self.cfg.timeout_secs
                        ));
                        told = Some(cur.nonce.clone());
                    }
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(200));
                }
            }
        }
    }

    /// Acquire several locks in sorted order (so two writers can never
    /// deadlock). If one fails, the ones already held are released.
    pub fn acquire_all(
        &self,
        names: &[String],
        holder: &str,
        human: Option<&str>,
    ) -> Result<Vec<LockGuard>> {
        let mut sorted: Vec<&String> = names.iter().collect();
        sorted.sort();
        sorted.dedup();
        let mut held = Vec::new();
        for n in sorted {
            held.push(self.acquire(n, holder, human)?);
        }
        Ok(held)
    }

    /// The current holder of a lock, if any.
    pub fn holder(&self, name: &str) -> Result<Option<LockInfo>> {
        self.guarded(name, || self.read(name))
    }

    fn renew(&self, info: &LockInfo) -> Result<bool> {
        self.guarded(&info.name, || match self.read(&info.name)? {
            Some(cur) if cur.nonce == info.nonce => {
                let lease = chrono::Duration::from_std(self.cfg.lease()).unwrap_or_default();
                let mut next = cur.clone();
                next.lease_expiry = rfc3339(now() + lease);
                let tmp = self.dir.join(format!("{}.lock.{}", info.name, info.nonce));
                fs::write(
                    &tmp,
                    serde_json::to_string_pretty(&next).expect("serializable"),
                )?;
                fs::rename(&tmp, self.lock_path(&info.name))?;
                Ok(true)
            }
            _ => Ok(false),
        })
    }

    fn release(&self, info: &LockInfo) -> Result<()> {
        self.guarded(&info.name, || {
            if let Some(cur) = self.read(&info.name)? {
                if cur.nonce == info.nonce {
                    fs::remove_file(self.lock_path(&info.name))?;
                }
            }
            Ok(())
        })
    }
}

/// A held lock. Renews its lease in the background; released on drop.
pub struct LockGuard {
    locks: Locks,
    info: LockInfo,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    beat: Option<JoinHandle<()>>,
}

impl LockGuard {
    fn start(locks: Locks, info: LockInfo) -> LockGuard {
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let every = (locks.cfg.lease() / 3).max(Duration::from_millis(50));
        let beat = {
            let (locks, info, stop, lost) =
                (locks.clone(), info.clone(), stop.clone(), lost.clone());
            std::thread::spawn(move || {
                let tick = Duration::from_millis(10);
                let mut since = Duration::ZERO;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    since += tick;
                    if since >= every {
                        since = Duration::ZERO;
                        if !matches!(locks.renew(&info), Ok(true)) {
                            lost.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            })
        };
        LockGuard {
            locks,
            info,
            stop,
            lost,
            beat: Some(beat),
        }
    }

    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    /// Error if the lock was lost (e.g. this process was stalled past its
    /// lease and the lock was recovered by someone else).
    pub fn ensure_held(&self) -> Result<()> {
        if self.lost.load(Ordering::Relaxed) {
            return Err(Error::LockLost(self.info.name.clone()));
        }
        Ok(())
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.beat.take() {
            let _ = h.join();
        }
        let _ = self.locks.release(&self.info);
    }
}

/// A short-lived exclusive flock (e.g. around git index/HEAD updates).
/// Released when dropped or when the process dies.
pub struct FileMutex(File);

impl FileMutex {
    pub fn lock(path: &Path) -> Result<FileMutex> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        f.lock()?;
        Ok(FileMutex(f))
    }
}

impl Drop for FileMutex {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locks(dir: &Path, when_busy: WhenBusy, timeout: f64, lease: f64) -> Locks {
        fs::create_dir_all(dir.join("locks")).unwrap();
        Locks::new(
            dir.join("locks"),
            dir.join("lock.log"),
            LockConfig {
                when_busy,
                timeout_secs: timeout,
                lease_secs: lease,
                ..Default::default()
            },
            Arc::new(|_| {}),
        )
    }

    #[test]
    fn exclusive_then_released() {
        let t = tempfile::TempDir::new().unwrap();
        let l = locks(t.path(), WhenBusy::Fail, 0.0, 30.0);
        let g = l.acquire("main", "a", Some("Ada")).unwrap();
        let err = l.acquire("main", "b", None).err().unwrap();
        assert!(err.to_string().contains("locked by a (Ada) since"), "{err}");
        drop(g);
        l.acquire("main", "b", None).unwrap();
    }

    #[test]
    fn dead_expired_lock_is_recovered_and_logged() {
        let t = tempfile::TempDir::new().unwrap();
        let l = locks(t.path(), WhenBusy::Wait, 5.0, 0.2);
        let stale = LockInfo {
            name: "main".into(),
            holder: "ghost".into(),
            human: None,
            pid: 999_999_999,
            host: hostname(),
            acquired: "2026-01-01T00:00:00Z".into(),
            lease_expiry: "2026-01-01T00:00:30Z".into(),
            nonce: "old".into(),
        };
        l.write_new(&stale).unwrap();
        let g = l.acquire("main", "b", None).unwrap();
        assert_eq!(g.info().holder, "b");
        let log = fs::read_to_string(t.path().join("lock.log")).unwrap();
        assert!(log.contains("recovered lock `main` from ghost"), "{log}");
    }

    #[test]
    fn live_holder_keeps_its_lock_past_the_lease() {
        let t = tempfile::TempDir::new().unwrap();
        let l = locks(t.path(), WhenBusy::Wait, 0.6, 0.15);
        let _g = l.acquire("main", "a", None).unwrap();
        // Lease is 150 ms, we wait 600 ms: the heartbeat must keep it.
        let err = l.acquire("main", "b", None).err().unwrap();
        assert!(matches!(err, Error::LockTimeout { .. }), "{err}");
    }

    #[test]
    fn expired_lock_from_a_live_pid_is_not_recovered() {
        let t = tempfile::TempDir::new().unwrap();
        let l = locks(t.path(), WhenBusy::Fail, 0.0, 30.0);
        l.write_new(&LockInfo {
            name: "main".into(),
            holder: "stalled".into(),
            human: None,
            pid: std::process::id(),
            host: hostname(),
            acquired: "2026-01-01T00:00:00Z".into(),
            lease_expiry: "2026-01-01T00:00:30Z".into(),
            nonce: "x".into(),
        })
        .unwrap();
        assert!(matches!(
            l.acquire("main", "b", None),
            Err(Error::Locked { .. })
        ));
    }
}
