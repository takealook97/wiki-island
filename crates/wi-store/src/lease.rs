//! The watcher lease. A running watcher (`wi watch`, the launchd
//! service, the watcher inside `wi mcp`) keeps `.git/wi/watcher-lease`
//! with its pid, host name and a heartbeat, refreshed every
//! [`LEASE_REFRESH`] and removed when it stops. One-shot commands skip
//! `auto_ingest` while a lease is fresh, because the watcher already
//! ingests and commits editor changes. A lease is fresh when its heartbeat
//! is at most [`LEASE_TTL`] old, it names this host, and its pid is alive.
//! Anything else (stale, foreign, unreadable) is ignored. The file lives
//! under `.git/`, so it is per device and never synced. Both intervals
//! are unmeasured policy choices.

use std::path::PathBuf;
use std::time::Duration;

use wi_history::Vault as HistoryVault;

const FILE: &str = "watcher-lease";
const MAGIC: &str = "wi-watcher-lease 1";

/// How often a live watcher rewrites its heartbeat.
pub const LEASE_REFRESH: Duration = Duration::from_secs(2);
/// How old a heartbeat may be before the lease counts as stale.
pub const LEASE_TTL: Duration = Duration::from_secs(10);

/// Parsed lease file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseRecord {
    pid: i32,
    host: String,
    heartbeat_ms: u64,
    token: String,
}

impl LeaseRecord {
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut field = |name: &str| {
            lines
                .next()?
                .strip_prefix(name)?
                .strip_prefix('\t')
                .map(str::to_owned)
        };
        Some(Self {
            pid: field("pid")?.parse().ok()?,
            host: field("host")?,
            heartbeat_ms: field("heartbeat_ms")?.parse().ok()?,
            token: field("token")?,
        })
    }

    fn render(&self) -> String {
        format!(
            "{MAGIC}\npid\t{}\nhost\t{}\nheartbeat_ms\t{}\ntoken\t{}\n",
            self.pid, self.host, self.heartbeat_ms, self.token
        )
    }

    fn is_fresh(&self, now_ms: u64, host: &str) -> bool {
        let ttl = u64::try_from(LEASE_TTL.as_millis()).unwrap_or(u64::MAX);
        self.host == host
            && now_ms.abs_diff(self.heartbeat_ms) <= ttl
            && !crate::doctor::process_gone(Some(self.pid))
    }
}

fn lease_path(history: &HistoryVault) -> PathBuf {
    history.wi_dir().join(FILE)
}

/// This machine's host name (empty when unknown).
fn host_name() -> String {
    #[cfg(unix)]
    {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

fn read(history: &HistoryVault) -> Option<LeaseRecord> {
    LeaseRecord::parse(&std::fs::read_to_string(lease_path(history)).ok()?)
}

/// Whether a live watcher on this host holds the lease.
pub(crate) fn held(history: &HistoryVault) -> bool {
    read(history).is_some_and(|r| r.is_fresh(crate::work::now_ms(), &host_name()))
}

/// A lease this process holds. Dropping it removes the file when it is
/// still ours.
#[derive(Debug)]
pub(crate) struct Lease {
    path: PathBuf,
    record: LeaseRecord,
}

impl Lease {
    /// Write a fresh lease. A failure only means one-shot commands keep
    /// ingesting, so it is returned for reporting, not fatal.
    pub(crate) fn acquire(history: &HistoryVault) -> std::io::Result<Self> {
        let lease = Self {
            path: lease_path(history),
            record: LeaseRecord {
                pid: i32::try_from(std::process::id()).unwrap_or(i32::MAX),
                host: host_name(),
                heartbeat_ms: crate::work::now_ms(),
                token: uuid::Uuid::new_v4().simple().to_string(),
            },
        };
        lease.write()?;
        Ok(lease)
    }

    /// Rewrite the heartbeat (also restores a lease another watcher
    /// removed when it stopped).
    pub(crate) fn refresh(&mut self) -> std::io::Result<()> {
        self.record.heartbeat_ms = crate::work::now_ms();
        self.write()
    }

    fn write(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self
            .path
            .with_extension(format!("tmp-{}-{}", self.record.pid, self.record.token));
        std::fs::write(&tmp, self.record.render())?;
        std::fs::rename(&tmp, &self.path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| LeaseRecord::parse(&t))
            .is_some_and(|r| r.token == self.record.token);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: i32, heartbeat_ms: u64) -> LeaseRecord {
        LeaseRecord {
            pid,
            host: "h".into(),
            heartbeat_ms,
            token: "t".into(),
        }
    }

    #[test]
    fn record_round_trips_and_rejects_garbage() {
        let r = record(42, 7);
        assert_eq!(LeaseRecord::parse(&r.render()), Some(r));
        assert_eq!(LeaseRecord::parse("garbage"), None);
        assert_eq!(LeaseRecord::parse(MAGIC), None);
    }

    #[test]
    fn freshness_needs_live_pid_same_host_and_recent_heartbeat() {
        let me = i32::try_from(std::process::id()).unwrap();
        let now = 1_000_000;
        let ttl = u64::try_from(LEASE_TTL.as_millis()).unwrap();
        assert!(record(me, now).is_fresh(now, "h"));
        assert!(record(me, now - ttl).is_fresh(now, "h"));
        assert!(!record(me, now - ttl - 1).is_fresh(now, "h"));
        assert!(!record(me, now).is_fresh(now, "other"));
    }
}
