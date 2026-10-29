//! Infinity session management for Herdr.
//!
//! The observer in `infinity.rs` answers "what is this pane doing?". This
//! module answers the complementary question: "what sessions exist, and how do
//! I open one?". It reads the same `~/.infinity/sessions.json` store, joins it
//! with whatever the daemon reports, and renders the result for the plugin's
//! actions and session-picker pane.
//!
//! Everything here is read-only with respect to Infinity. Opening a session
//! means asking Herdr to create a pane that runs `infinity -s <id>`; we never
//! ask Infinity's daemon to create, resume, or stop anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::infinity::{self, SessionEntry};

/// How long to wait for the daemon to describe its live sessions before
/// falling back to the on-disk store alone.
const DAEMON_POLL: Duration = Duration::from_millis(1500);

/// A session as the management surface sees it: the store's durable record,
/// enriched with the daemon's live status when the daemon is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    /// `Running`, `Idle`, `WaitingForChoice`, ... or `None` when the daemon did
    /// not mention this session (it is on disk but not live).
    pub status: Option<String>,
    pub last_updated: Option<String>,
    pub archived: bool,
}

impl Session {
    /// The label shown in the picker. Falls back to the store's flags when the
    /// daemon is not running, so the list is still useful offline.
    pub fn status_label(&self) -> &str {
        match self.status.as_deref() {
            Some(status) => status,
            None if self.archived => "Archived",
            None => "Stored",
        }
    }

    pub fn is_live(&self) -> bool {
        !self.archived && !matches!(self.status.as_deref(), Some("Archived"))
    }
}

/// Collect sessions from the store, joined with the daemon's live view.
///
/// `daemon` may be empty: a missing or unreachable daemon degrades the listing
/// to the store's own flags rather than failing.
pub fn collect(
    store: &HashMap<String, SessionEntry>,
    daemon: &HashMap<String, infinity::SessionInfo>,
) -> Vec<Session> {
    let mut sessions: Vec<Session> = store
        .iter()
        .map(|(id, entry)| Session {
            id: id.clone(),
            cwd: entry.cwd.clone(),
            status: daemon.get(id).map(|info| info.status.clone()),
            last_updated: daemon.get(id).map(|info| info.last_updated.clone()),
            archived: entry.archived,
        })
        .collect();

    // A session the daemon knows about but that has not been flushed to the
    // store yet still belongs in the list.
    for (id, info) in daemon {
        if !store.contains_key(id) {
            sessions.push(Session {
                id: id.clone(),
                cwd: PathBuf::new(),
                status: Some(info.status.clone()),
                last_updated: Some(info.last_updated.clone()),
                archived: info.status == "Archived",
            });
        }
    }

    sort_sessions(&mut sessions);
    sessions
}

/// Newest first, with live sessions ahead of archived ones. Ties break on the
/// id so the order is stable between invocations.
fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|left, right| {
        left.archived
            .cmp(&right.archived)
            .then_with(|| {
                right
                    .last_updated
                    .as_deref()
                    .unwrap_or("")
                    .cmp(left.last_updated.as_deref().unwrap_or(""))
            })
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Restrict a listing to one directory. Used by the "sessions here" action so a
/// workspace shows only its own work.
pub fn in_directory(sessions: &[Session], dir: &Path) -> Vec<Session> {
    sessions
        .iter()
        .filter(|session| same_dir(&session.cwd, dir))
        .cloned()
        .collect()
}

fn same_dir(left: &Path, right: &Path) -> bool {
    if left.as_os_str().is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Resolve a user-supplied prefix to exactly one session.
///
/// Ambiguity is an error rather than a guess: the management surface acts on
/// the result, so picking the wrong session would open the wrong conversation.
pub fn resolve_prefix(sessions: &[Session], prefix: &str) -> Result<Session, String> {
    if prefix.is_empty() {
        return Err("a session id or prefix is required".to_owned());
    }
    if let Some(exact) = sessions.iter().find(|session| session.id == prefix) {
        return Ok(exact.clone());
    }

    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.id.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no session matches {prefix:?}")),
        [only] => Ok((*only).clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|session| session.id.as_str()).collect();
            Err(format!(
                "{prefix:?} matches {} sessions: {}",
                many.len(),
                ids.join(", ")
            ))
        }
    }
}

/// Read the store and ask the daemon for its live view, then join the two.
pub fn load() -> Vec<Session> {
    let store = infinity::read_store_sessions();
    let daemon = infinity::probe_daemon_sessions(DAEMON_POLL).unwrap_or_default();
    collect(&store, &daemon)
}

/// Render a listing as aligned text for the picker pane and `sessions` output.
pub fn render_table(sessions: &[Session]) -> String {
    if sessions.is_empty() {
        return "No Infinity sessions found.\n".to_owned();
    }

    let id_width = sessions
        .iter()
        .map(|session| session.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let status_width = sessions
        .iter()
        .map(|session| session.status_label().len())
        .max()
        .unwrap_or(6)
        .max(6);

    let mut out = format!(
        "{:<id_width$}  {:<status_width$}  {}\n",
        "ID",
        "STATUS",
        "DIRECTORY",
        id_width = id_width,
        status_width = status_width
    );
    for session in sessions {
        let cwd = if session.cwd.as_os_str().is_empty() {
            "<unknown>".to_owned()
        } else {
            session.cwd.display().to_string()
        };
        out.push_str(&format!(
            "{:<id_width$}  {:<status_width$}  {}\n",
            session.id,
            session.status_label(),
            cwd,
            id_width = id_width,
            status_width = status_width
        ));
    }
    out
}

/// The argv that opens a session in a pane, as `herdr pane run` needs it.
///
/// Routing through `herdr-infinity run` rather than bare `infinity` is what
/// keeps state reporting alive for the newly opened pane.
pub fn open_command(session: Option<&str>) -> String {
    match session {
        Some(id) => format!("herdr-infinity run -- infinity -s {id}"),
        None => "herdr-infinity run -- infinity".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cwd: &str, archived: bool) -> SessionEntry {
        SessionEntry {
            cwd: PathBuf::from(cwd),
            shut_down: false,
            idle: false,
            archived,
        }
    }

    fn info(status: &str, updated: &str) -> infinity::SessionInfo {
        infinity::SessionInfo {
            last_updated: updated.to_owned(),
            status: status.to_owned(),
        }
    }

    #[test]
    fn joins_store_with_daemon_status() {
        let mut store = HashMap::new();
        store.insert("aaa".to_owned(), entry("/work/one", false));
        store.insert("bbb".to_owned(), entry("/work/two", false));
        let mut daemon = HashMap::new();
        daemon.insert("aaa".to_owned(), info("Running", "2026-10-29T10:00:00Z"));

        let sessions = collect(&store, &daemon);
        assert_eq!(sessions.len(), 2);
        let aaa = sessions.iter().find(|s| s.id == "aaa").unwrap();
        assert_eq!(aaa.status.as_deref(), Some("Running"));
        assert_eq!(aaa.status_label(), "Running");
        // Not mentioned by the daemon: still listed, labelled from the store.
        let bbb = sessions.iter().find(|s| s.id == "bbb").unwrap();
        assert_eq!(bbb.status, None);
        assert_eq!(bbb.status_label(), "Stored");
    }

    #[test]
    fn includes_sessions_the_store_has_not_flushed() {
        let store = HashMap::new();
        let mut daemon = HashMap::new();
        daemon.insert("fresh".to_owned(), info("Running", "2026-10-29T12:00:00Z"));

        let sessions = collect(&store, &daemon);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "fresh");
        assert!(sessions[0].cwd.as_os_str().is_empty());
    }

    #[test]
    fn live_sessions_sort_before_archived_newest_first() {
        let mut store = HashMap::new();
        store.insert("old".to_owned(), entry("/work", false));
        store.insert("new".to_owned(), entry("/work", false));
        store.insert("gone".to_owned(), entry("/work", true));
        let mut daemon = HashMap::new();
        daemon.insert("old".to_owned(), info("Idle", "2026-10-29T09:00:00Z"));
        daemon.insert("new".to_owned(), info("Idle", "2026-10-29T18:00:00Z"));

        let sessions = collect(&store, &daemon);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "old", "gone"]);
        assert!(!sessions[2].is_live());
    }

    #[test]
    fn resolve_prefix_requires_a_unique_match() {
        let sessions = vec![
            Session {
                id: "abc-1".to_owned(),
                cwd: PathBuf::from("/work"),
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "abc-2".to_owned(),
                cwd: PathBuf::from("/work"),
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "xyz".to_owned(),
                cwd: PathBuf::from("/work"),
                status: None,
                last_updated: None,
                archived: false,
            },
        ];

        assert_eq!(resolve_prefix(&sessions, "xyz").unwrap().id, "xyz");
        // An exact id wins even when it prefixes another.
        assert_eq!(resolve_prefix(&sessions, "abc-1").unwrap().id, "abc-1");
        let error = resolve_prefix(&sessions, "abc").unwrap_err();
        assert!(error.contains("matches 2 sessions"), "{error}");
        assert!(resolve_prefix(&sessions, "nope").is_err());
        assert!(resolve_prefix(&sessions, "").is_err());
    }

    #[test]
    fn filters_by_directory_ignoring_unknown_cwd() {
        let sessions = vec![
            Session {
                id: "here".to_owned(),
                cwd: PathBuf::from("/work/project"),
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "elsewhere".to_owned(),
                cwd: PathBuf::from("/other"),
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "unknown".to_owned(),
                cwd: PathBuf::new(),
                status: None,
                last_updated: None,
                archived: false,
            },
        ];

        let here = in_directory(&sessions, Path::new("/work/project"));
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].id, "here");
    }

    #[test]
    fn renders_a_table_and_an_empty_notice() {
        assert!(render_table(&[]).contains("No Infinity sessions"));

        let sessions = vec![Session {
            id: "abc".to_owned(),
            cwd: PathBuf::from("/work/project"),
            status: Some("Running".to_owned()),
            last_updated: None,
            archived: false,
        }];
        let table = render_table(&sessions);
        assert!(table.contains("ID"));
        assert!(table.contains("abc"));
        assert!(table.contains("Running"));
        assert!(table.contains("/work/project"));
    }

    #[test]
    fn open_command_routes_through_the_wrapper() {
        assert_eq!(
            open_command(Some("abc")),
            "herdr-infinity run -- infinity -s abc"
        );
        assert_eq!(open_command(None), "herdr-infinity run -- infinity");
    }
}
