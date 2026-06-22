//! A bounded, in-memory ring buffer of log entries captured from a connected
//! Roblox Studio session — its Output: prints, warnings, and errors (with stack
//! traces).
//!
//! The Studio plugin captures `LogService` output and POSTs batches to
//! `/api/feedback`; they land here, owned by the [`ServeSession`]. The CLI
//! (`rojo logs`) and the MCP `read_logs` tool pull a filtered snapshot back out
//! via `/api/logs`. This is the server half of the runtime-feedback loop: it
//! turns Rojo from "push-only" (files → Studio) into something an agent can also
//! *observe* (Studio → agent).
//!
//! The buffer is bounded so a noisy game can't grow it without limit — the
//! oldest entries are dropped first, and a running `dropped` count plus the
//! monotonic `seq` let a reader detect that it missed some.

use std::{collections::VecDeque, sync::Mutex};

use serde::{Deserialize, Serialize};

/// Maximum number of entries retained before the oldest are dropped.
const DEFAULT_CAPACITY: usize = 2000;

/// Maximum length (in bytes) of a single entry's message. Longer messages are
/// truncated with a marker, mirroring the cap applied to test output in
/// `src/cli/test.rs`.
const MAX_MESSAGE_LEN: usize = 8 * 1024;

/// The severity of a captured log line, mirrored from Roblox's `Enum.MessageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Print,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    /// Parses a wire-format level string (case-insensitive), defaulting unknown
    /// values to `Print` so a slightly-off client can't have its logs rejected.
    pub fn from_wire(s: &str) -> LogLevel {
        match s.to_ascii_lowercase().as_str() {
            "error" => LogLevel::Error,
            "warning" | "warn" => LogLevel::Warning,
            "info" => LogLevel::Info,
            _ => LogLevel::Print,
        }
    }

    /// Rank used for `level_at_least` filtering; higher is more severe.
    fn severity(self) -> u8 {
        match self {
            LogLevel::Print => 0,
            LogLevel::Info => 1,
            LogLevel::Warning => 2,
            LogLevel::Error => 3,
        }
    }

    /// The wire-format string for this level (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Print => "print",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
        }
    }
}

/// Which Studio run context produced a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunMode {
    Edit,
    Client,
    Server,
    Unknown,
}

impl RunMode {
    /// Parses a wire-format run-mode string (case-insensitive), defaulting
    /// unknown values to `Unknown`.
    pub fn from_wire(s: &str) -> RunMode {
        match s.to_ascii_lowercase().as_str() {
            "edit" => RunMode::Edit,
            "client" => RunMode::Client,
            "server" => RunMode::Server,
            _ => RunMode::Unknown,
        }
    }

    /// The wire-format string for this run mode (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Edit => "edit",
            RunMode::Client => "client",
            RunMode::Server => "server",
            RunMode::Unknown => "unknown",
        }
    }
}

/// A single captured log line, with a server-assigned monotonic sequence number.
/// This is also the wire shape returned by `/api/logs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub message: String,
    pub run_mode: RunMode,
}

/// An entry as received from the plugin, before the server assigns a `seq`.
pub struct IncomingLogEntry {
    pub timestamp_unix_ms: u64,
    pub level: LogLevel,
    pub message: String,
    pub run_mode: RunMode,
}

/// The result of a [`LogBuffer::snapshot`] query.
pub struct LogSnapshot {
    /// Entries matching the query, oldest first.
    pub entries: Vec<LogEntry>,
    /// The seq of the oldest entry still retained (0 if empty).
    pub head_seq: u64,
    /// The seq that will be assigned to the next entry pushed (i.e. one past the
    /// newest). A reader can pass this as `since` next time to get only newer
    /// entries.
    pub tail_seq: u64,
    /// How many entries have ever been dropped to stay within capacity.
    pub dropped: u64,
}

/// A bounded ring buffer of [`LogEntry`]s. See the module docs.
pub struct LogBuffer {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    /// Monotonic counter; the seq for the next entry to be pushed.
    next_seq: u64,
    /// Total entries dropped to stay within capacity.
    dropped: u64,
}

impl LogBuffer {
    pub fn new() -> LogBuffer {
        LogBuffer::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> LogBuffer {
        LogBuffer {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                next_seq: 0,
                dropped: 0,
            }),
            capacity: capacity.max(1),
        }
    }

    /// Appends a batch of entries, assigning each a monotonic `seq`. The oldest
    /// entries are evicted (and counted in `dropped`) once over capacity.
    /// Returns the number of entries accepted.
    pub fn push_batch(&self, incoming: impl IntoIterator<Item = IncomingLogEntry>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let mut accepted = 0;

        for entry in incoming {
            let message = truncate(entry.message, MAX_MESSAGE_LEN);
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.entries.push_back(LogEntry {
                seq,
                timestamp_unix_ms: entry.timestamp_unix_ms,
                level: entry.level,
                message,
                run_mode: entry.run_mode,
            });
            accepted += 1;

            while inner.entries.len() > self.capacity {
                inner.entries.pop_front();
                inner.dropped += 1;
            }
        }

        accepted
    }

    /// Returns the entries newer than `since_seq` (exclusive) and at least
    /// `level_at_least` severe. When more than `limit` match, the **newest**
    /// `limit` are returned (tail semantics, like `tail -n`), oldest first.
    pub fn snapshot(
        &self,
        since_seq: Option<u64>,
        level_at_least: Option<LogLevel>,
        limit: Option<usize>,
    ) -> LogSnapshot {
        let inner = self.inner.lock().unwrap();

        let min_severity = level_at_least.map(LogLevel::severity).unwrap_or(0);

        let mut entries: Vec<LogEntry> = inner
            .entries
            .iter()
            .filter(|entry| match since_seq {
                Some(since) => entry.seq > since,
                None => true,
            })
            .filter(|entry| entry.level.severity() >= min_severity)
            .cloned()
            .collect();

        if let Some(limit) = limit {
            if entries.len() > limit {
                // Keep the newest `limit` entries (drop from the front),
                // preserving oldest-first order within the result.
                let drop_count = entries.len() - limit;
                entries.drain(..drop_count);
            }
        }

        LogSnapshot {
            entries,
            head_seq: inner.entries.front().map(|entry| entry.seq).unwrap_or(0),
            tail_seq: inner.next_seq,
            dropped: inner.dropped,
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        LogBuffer::new()
    }
}

/// Truncates `message` to at most `max_len` bytes on a char boundary, appending
/// a marker when it had to cut. Mirrors the cap in `src/cli/test.rs`.
fn truncate(mut message: String, max_len: usize) -> String {
    const MARKER: &str = "…[truncated]";

    if message.len() <= max_len {
        return message;
    }

    let mut end = max_len.saturating_sub(MARKER.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(MARKER);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(message: &str, level: LogLevel) -> IncomingLogEntry {
        IncomingLogEntry {
            timestamp_unix_ms: 0,
            level,
            message: message.to_owned(),
            run_mode: RunMode::Edit,
        }
    }

    #[test]
    fn push_assigns_monotonic_seq() {
        let buffer = LogBuffer::new();
        buffer.push_batch([entry("a", LogLevel::Print), entry("b", LogLevel::Print)]);

        let snapshot = buffer.snapshot(None, None, None);
        let seqs: Vec<u64> = snapshot.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
        assert_eq!(snapshot.tail_seq, 2);
        assert_eq!(snapshot.dropped, 0);
    }

    #[test]
    fn over_capacity_drops_oldest_and_counts() {
        let buffer = LogBuffer::with_capacity(2);
        buffer.push_batch([
            entry("a", LogLevel::Print),
            entry("b", LogLevel::Print),
            entry("c", LogLevel::Print),
        ]);

        let snapshot = buffer.snapshot(None, None, None);
        let messages: Vec<&str> = snapshot
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages, vec!["b", "c"]);
        assert_eq!(snapshot.dropped, 1);
        assert_eq!(snapshot.head_seq, 1);
        assert_eq!(snapshot.tail_seq, 3);
    }

    #[test]
    fn since_seq_returns_only_newer() {
        let buffer = LogBuffer::new();
        buffer.push_batch([
            entry("a", LogLevel::Print),
            entry("b", LogLevel::Print),
            entry("c", LogLevel::Print),
        ]);

        let snapshot = buffer.snapshot(Some(0), None, None);
        let messages: Vec<&str> = snapshot
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages, vec!["b", "c"]);
    }

    #[test]
    fn level_filter_keeps_at_least_as_severe() {
        let buffer = LogBuffer::new();
        buffer.push_batch([
            entry("info", LogLevel::Info),
            entry("warn", LogLevel::Warning),
            entry("err", LogLevel::Error),
            entry("print", LogLevel::Print),
        ]);

        let snapshot = buffer.snapshot(None, Some(LogLevel::Warning), None);
        let messages: Vec<&str> = snapshot
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages, vec!["warn", "err"]);
    }

    #[test]
    fn limit_keeps_newest() {
        let buffer = LogBuffer::new();
        buffer.push_batch([
            entry("a", LogLevel::Print),
            entry("b", LogLevel::Print),
            entry("c", LogLevel::Print),
        ]);

        let snapshot = buffer.snapshot(None, None, Some(2));
        let messages: Vec<&str> = snapshot
            .entries
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages, vec!["b", "c"]);
    }

    #[test]
    fn long_message_is_truncated() {
        let buffer = LogBuffer::new();
        let long = "x".repeat(MAX_MESSAGE_LEN * 2);
        buffer.push_batch([entry(&long, LogLevel::Print)]);

        let snapshot = buffer.snapshot(None, None, None);
        let message = &snapshot.entries[0].message;
        assert!(message.len() <= MAX_MESSAGE_LEN);
        assert!(message.ends_with("…[truncated]"));
    }

    #[test]
    fn level_from_wire_is_forgiving() {
        assert_eq!(LogLevel::from_wire("Error"), LogLevel::Error);
        assert_eq!(LogLevel::from_wire("warn"), LogLevel::Warning);
        assert_eq!(LogLevel::from_wire("nonsense"), LogLevel::Print);
        assert_eq!(RunMode::from_wire("Server"), RunMode::Server);
        assert_eq!(RunMode::from_wire("nonsense"), RunMode::Unknown);
    }
}
