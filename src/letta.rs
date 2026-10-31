//! Observation of a Letta Code session.
//!
//! Infinity publishes state on a daemon socket, so `infinity.rs` can subscribe
//! and be told when anything changes. Letta has no such socket: the local
//! backend's state lives on disk under
//! `~/.letta/lc-local-backend/`. So this observer polls instead, and infers
//! the agent's state from the shape of the conversation log rather than being
//! told it.
//!
//! The log is newline-delimited JSON, one entry per line, written as the turn
//! progresses:
//!
//! ```text
//! {"type":"session","id":"local-conv-1","cwd":"/work",...}
//! {"type":"message","message":{"role":"user","content":[...]},...}
//! {"type":"message","message":{"role":"assistant","content":[...]},...}
//! ```
//!
//! The last `message` entry's role is the signal: a trailing `user` message
//! means Letta is composing a reply (`working`); a trailing `assistant`
//! message means it is waiting for input (`idle`). That is a weaker signal
//! than Infinity's explicit status — it cannot see an approval prompt, so
//! `blocked` is never reported — and it is deliberately conservative: an
//! unreadable or truncated log leaves the last state alone rather than
//! guessing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::herdr::AgentState;

/// How long a trailing user message may stand before the observer stops
/// claiming the agent is working.
///
/// A turn that fails, or a TUI killed mid-reply, leaves a user message as the
/// last entry forever. Without a cap the pane would show `working` until the
/// pane closed.
const WORKING_GRACE: Duration = Duration::from_secs(30 * 60);

/// A conversation recorded by the local backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub agent_id: String,
    pub dir: PathBuf,
    pub cwd: Option<PathBuf>,
    pub archived: bool,
    /// `last_message_at`, used to pick the most recent of several.
    pub last_message_at: String,
}

#[derive(Debug, Deserialize)]
struct ConversationFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    last_message_at: String,
}

/// The root of the local backend's state.
pub fn backend_dir() -> PathBuf {
    crate::home_dir().join(".letta/lc-local-backend")
}

/// Read every conversation the local backend knows about.
///
/// A conversation whose `conversation.json` is missing or malformed is skipped
/// rather than failing the scan: the directory may be mid-write.
pub fn conversations(backend: &Path) -> Vec<Conversation> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(backend.join("conversations")) else {
        return found;
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(contents) = std::fs::read_to_string(dir.join("conversation.json")) else {
            continue;
        };
        let Ok(file) = serde_json::from_str::<ConversationFile>(&contents) else {
            continue;
        };
        if file.id.is_empty() {
            continue;
        }
        found.push(Conversation {
            id: file.id,
            agent_id: file.agent_id,
            cwd: session_cwd(&dir),
            dir,
            archived: file.archived,
            last_message_at: file.last_message_at,
        });
    }

    // Newest first, so callers can take the first match.
    found.sort_by(|left, right| {
        right
            .last_message_at
            .cmp(&left.last_message_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    found
}

/// The working directory recorded in the log's `session` entry.
///
/// Letta writes it once, at the top of `messages.jsonl`, and it is the only
/// link between a conversation and the pane that started it.
fn session_cwd(dir: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dir.join("messages.jsonl")).ok()?;
    for line in contents.lines().take(4) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) == Some("session") {
            if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

/// Pick the conversation belonging to a pane.
///
/// Preference order mirrors `infinity.rs`: an explicit id, then the newest
/// live conversation whose recorded cwd matches the pane. Two Letta sessions
/// in one directory cannot be told apart, so the newest wins.
pub fn select<'a>(
    conversations: &'a [Conversation],
    explicit: Option<&str>,
    pane_cwd: &Path,
) -> Option<&'a Conversation> {
    if let Some(wanted) = explicit {
        return conversations
            .iter()
            .find(|c| c.id == wanted || c.agent_id == wanted);
    }
    conversations
        .iter()
        .find(|c| !c.archived && c.cwd.as_deref().map(|d| same_dir(d, pane_cwd)) == Some(true))
}

fn same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// The role of the last message in a conversation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastRole {
    User,
    Assistant,
    /// No message entries yet: the session line only.
    None,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    message: Option<LogMessage>,
}

#[derive(Debug, Deserialize)]
struct LogMessage {
    #[serde(default)]
    role: String,
}

/// Find the role of the final `message` entry in a log.
///
/// Entries other than `message` — the `session` header, compaction records —
/// are skipped, and an unparseable line is ignored rather than ending the
/// scan, since the last line may be half-written.
pub fn last_role(log: &str) -> LastRole {
    let mut role = LastRole::None;
    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<LogEntry>(line) else {
            continue;
        };
        if entry.r#type != "message" {
            continue;
        }
        match entry.message.as_ref().map(|m| m.role.as_str()) {
            Some("user") => role = LastRole::User,
            Some("assistant") => role = LastRole::Assistant,
            _ => {}
        }
    }
    role
}

/// Infer the agent state from a log and how long ago it changed.
///
/// `age` is the time since the log was last written. A trailing user message
/// means a reply is in flight, but only for as long as the grace period: past
/// that the turn is assumed dead and the agent is reported idle, so a pane
/// does not sit on `working` indefinitely.
pub fn state_for(role: LastRole, age: Duration) -> Option<AgentState> {
    match role {
        LastRole::User if age < WORKING_GRACE => Some(AgentState::Working),
        // A stale user message means the turn never completed.
        LastRole::User => Some(AgentState::Idle),
        LastRole::Assistant => Some(AgentState::Idle),
        // Claim the pane even before the first turn, so Letta appears in the
        // sidebar as soon as it starts.
        LastRole::None => Some(AgentState::Idle),
    }
}

/// Read a conversation's log and decide its state.
pub fn observe(conversation: &Conversation) -> Option<AgentState> {
    let path = conversation.dir.join("messages.jsonl");
    let log = std::fs::read_to_string(&path).ok()?;
    let age = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or_default();
    state_for(last_role(&log), age)
}

/// Every agent's MemFS directory, keyed by agent id.
///
/// Used by `memfs` to find a live agent's memory without being told a path.
pub fn memfs_dirs(backend: &Path) -> HashMap<String, PathBuf> {
    let mut found = HashMap::new();
    let Ok(entries) = std::fs::read_dir(backend.join("memfs")) else {
        return found;
    };
    for entry in entries.flatten() {
        let memory = entry.path().join("memory");
        if !memory.is_dir() {
            continue;
        }
        if let Some(agent) = entry.file_name().to_str() {
            found.insert(agent.to_owned(), memory);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str =
        r#"{"type":"session","version":3,"id":"local-conv-1","cwd":"/work/project"}"#;
    const USER: &str = r#"{"type":"message","message":{"role":"user","content":[]}}"#;
    const ASSISTANT: &str = r#"{"type":"message","message":{"role":"assistant","content":[]}}"#;

    fn write_conversation(root: &Path, name: &str, id: &str, cwd: &str, log: &str) -> PathBuf {
        let dir = root.join("conversations").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("conversation.json"),
            format!(
                r#"{{"id":"{id}","agent_id":"agent-{id}","archived":false,
                     "last_message_at":"2026-10-31T10:00:00Z"}}"#
            ),
        )
        .unwrap();
        let header = format!(r#"{{"type":"session","version":3,"id":"{id}","cwd":"{cwd}"}}"#);
        std::fs::write(dir.join("messages.jsonl"), format!("{header}\n{log}")).unwrap();
        dir
    }

    #[test]
    fn reads_the_trailing_role() {
        assert_eq!(last_role(&format!("{SESSION}\n{USER}")), LastRole::User);
        assert_eq!(
            last_role(&format!("{SESSION}\n{USER}\n{ASSISTANT}")),
            LastRole::Assistant
        );
        // A session header alone is not a turn.
        assert_eq!(last_role(SESSION), LastRole::None);
        assert_eq!(last_role(""), LastRole::None);
    }

    #[test]
    fn ignores_non_message_entries_and_bad_lines() {
        let log = format!(
            "{SESSION}\n{USER}\n{ASSISTANT}\n\
             {{\"type\":\"compaction\",\"tokensBefore\":460}}\n\
             not json at all\n"
        );
        // Compaction and garbage must not change the answer.
        assert_eq!(last_role(&log), LastRole::Assistant);

        // A half-written final line is ignored, not fatal.
        let partial = format!("{SESSION}\n{ASSISTANT}\n{{\"type\":\"mess");
        assert_eq!(last_role(&partial), LastRole::Assistant);
    }

    #[test]
    fn maps_roles_to_states_with_a_grace_period() {
        let fresh = Duration::from_secs(5);
        assert_eq!(state_for(LastRole::User, fresh), Some(AgentState::Working));
        assert_eq!(
            state_for(LastRole::Assistant, fresh),
            Some(AgentState::Idle)
        );
        // Claim the pane before the first turn.
        assert_eq!(state_for(LastRole::None, fresh), Some(AgentState::Idle));

        // A user message left standing means the turn died; do not report
        // working forever.
        let stale = WORKING_GRACE + Duration::from_secs(1);
        assert_eq!(state_for(LastRole::User, stale), Some(AgentState::Idle));
    }

    #[test]
    fn lists_conversations_newest_first() {
        let root = tempfile::tempdir().unwrap();
        write_conversation(root.path(), "a", "conv-a", "/work/one", USER);
        let dir_b = root.path().join("conversations/b");
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(
            dir_b.join("conversation.json"),
            r#"{"id":"conv-b","agent_id":"agent-b","archived":false,
                "last_message_at":"2026-10-31T23:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(dir_b.join("messages.jsonl"), SESSION).unwrap();

        let found = conversations(root.path());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "conv-b", "newest should sort first");
        assert_eq!(found[1].cwd.as_deref(), Some(Path::new("/work/one")));
    }

    #[test]
    fn skips_a_malformed_conversation_without_failing_the_scan() {
        let root = tempfile::tempdir().unwrap();
        write_conversation(root.path(), "good", "conv-good", "/work", ASSISTANT);
        let bad = root.path().join("conversations/bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("conversation.json"), "{ truncated").unwrap();

        let found = conversations(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "conv-good");
    }

    #[test]
    fn a_missing_backend_is_empty_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(conversations(&root.path().join("absent")).is_empty());
        assert!(memfs_dirs(&root.path().join("absent")).is_empty());
    }

    #[test]
    fn selects_by_explicit_id_or_matching_directory() {
        let root = tempfile::tempdir().unwrap();
        write_conversation(root.path(), "a", "conv-a", "/work/one", USER);
        write_conversation(root.path(), "b", "conv-b", "/work/two", USER);
        let found = conversations(root.path());

        // An explicit id wins regardless of directory.
        assert_eq!(
            select(&found, Some("conv-a"), Path::new("/elsewhere")).map(|c| c.id.as_str()),
            Some("conv-a")
        );
        // The agent id resolves too, since that is what `--agent` takes.
        assert_eq!(
            select(&found, Some("agent-conv-b"), Path::new("/elsewhere")).map(|c| c.id.as_str()),
            Some("conv-b")
        );
        // Otherwise the pane's directory decides.
        assert_eq!(
            select(&found, None, Path::new("/work/two")).map(|c| c.id.as_str()),
            Some("conv-b")
        );
        assert!(select(&found, None, Path::new("/unrelated")).is_none());
        assert!(select(&found, Some("nope"), Path::new("/work/one")).is_none());
    }

    #[test]
    fn an_archived_conversation_is_not_selected_by_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("conversations/old");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("conversation.json"),
            r#"{"id":"gone","agent_id":"a","archived":true,
                "last_message_at":"2026-10-31T10:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("messages.jsonl"),
            r#"{"type":"session","cwd":"/work"}"#,
        )
        .unwrap();

        let found = conversations(root.path());
        assert_eq!(found.len(), 1);
        assert!(select(&found, None, Path::new("/work")).is_none());
    }

    #[test]
    fn observes_a_conversation_on_disk() {
        let root = tempfile::tempdir().unwrap();
        write_conversation(root.path(), "a", "conv-a", "/work", USER);
        let found = conversations(root.path());
        // Freshly written, so a trailing user message means working.
        assert_eq!(observe(&found[0]), Some(AgentState::Working));

        write_conversation(root.path(), "a", "conv-a", "/work", ASSISTANT);
        let found = conversations(root.path());
        assert_eq!(observe(&found[0]), Some(AgentState::Idle));
    }

    #[test]
    fn finds_memfs_directories_by_agent() {
        let root = tempfile::tempdir().unwrap();
        let memory = root.path().join("memfs/agent-local-1/memory");
        std::fs::create_dir_all(&memory).unwrap();
        // A sibling without a memory/ subdirectory must be ignored.
        std::fs::create_dir_all(root.path().join("memfs/agent-local-2")).unwrap();

        let dirs = memfs_dirs(root.path());
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs.get("agent-local-1"), Some(&memory));
    }
}
