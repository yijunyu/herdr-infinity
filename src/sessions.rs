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
    /// The host `cwd` lives on. `None` means nothing told us, which is why
    /// the pane prints the directory unqualified rather than assuming local:
    /// a session on `ada` whose path also exists here would otherwise read
    /// as a local folder.
    pub host: Option<String>,
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

/// The remote name Infinity itself prefixes onto a remote session's id.
///
/// Infinity names remotes in `~/.infinity/remotes.json` ("a short label for
/// this remote; shows as a prefix in the session picker") and shows a remote
/// session as `devbox/def456`. So the host is already carried IN THE ID, and
/// herdr was dropping it: the id went into the table verbatim while the cwd
/// beside it -- a path on `devbox` -- was printed as if it were local.
///
/// Returns `(host, bare_id)`, or `None` when the id has no prefix, which is
/// what a local session looks like. Only a single leading segment counts: a
/// session id is opaque otherwise, and splitting on every slash would invent
/// a host out of any id that happens to contain one.
fn split_remote_id(id: &str) -> Option<(String, String)> {
    let (host, rest) = id.split_once('/')?;
    if host.is_empty() || rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some((host.to_owned(), rest.to_owned()))
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
            // The daemon's live view wins over the store's record: a session
            // that migrated hosts has a stale host on disk until it flushes.
            host: daemon
                .get(id)
                .and_then(|info| info.host.clone())
                .or_else(|| entry.host.clone())
                .or_else(|| split_remote_id(id).map(|(host, _)| host)),
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
                host: info
                    .host
                    .clone()
                    .or_else(|| split_remote_id(id).map(|(host, _)| host)),
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
///
/// The HOST column shows the remote's label, which is what Infinity itself
/// prefixes onto the id. When `~/.infinity/remotes.json` also gives that
/// label an SSH target, it is shown in parentheses -- `ada (ada.example.com)`
/// -- because the label alone does not say which machine it reaches, and that
/// is the one thing remotes.json knows and the id does not.
pub fn render_table(sessions: &[Session]) -> String {
    render_table_with_remotes(sessions, &infinity::read_remotes())
}

/// `render_table`, with the remotes map injected so it can be tested without
/// a `~/.infinity/remotes.json` on the machine running the tests.
pub fn render_table_with_remotes(
    sessions: &[Session],
    remotes: &HashMap<String, infinity::Remote>,
) -> String {
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
    // Only widen for a HOST column when some session actually reports one.
    // With no remote sessions the table keeps its original three columns, so
    // the common local case is unchanged.
    let any_host = sessions.iter().any(|session| session.host.is_some());
    // The cell for a session's host: the label, plus the SSH target behind it
    // when remotes.json names one.
    let host_cell = |session: &Session| -> Option<String> {
        let label = session.host.as_deref()?;
        match remotes.get(label).and_then(infinity::Remote::ssh_target) {
            Some(target) if target != label => Some(format!("{label} ({target})")),
            _ => Some(label.to_owned()),
        }
    };
    let host_width = sessions
        .iter()
        .filter_map(host_cell)
        .map(|cell| cell.len())
        .max()
        .unwrap_or(4)
        .max(4);

    let mut out = if any_host {
        format!(
            "{:<id_width$}  {:<status_width$}  {:<host_width$}  {}\n",
            "ID",
            "STATUS",
            "HOST",
            "DIRECTORY",
            id_width = id_width,
            status_width = status_width,
            host_width = host_width
        )
    } else {
        format!(
            "{:<id_width$}  {:<status_width$}  {}\n",
            "ID",
            "STATUS",
            "DIRECTORY",
            id_width = id_width,
            status_width = status_width
        )
    };
    for session in sessions {
        let cwd = if session.cwd.as_os_str().is_empty() {
            "<unknown>".to_owned()
        } else {
            session.cwd.display().to_string()
        };
        if any_host {
            // "-" where the host is unreported. NOT "local": see Session::host.
            out.push_str(&format!(
                "{:<id_width$}  {:<status_width$}  {:<host_width$}  {}\n",
                session.id,
                session.status_label(),
                host_cell(session).as_deref().unwrap_or("-"),
                cwd,
                id_width = id_width,
                status_width = status_width,
                host_width = host_width
            ));
        } else {
            out.push_str(&format!(
                "{:<id_width$}  {:<status_width$}  {}\n",
                session.id,
                session.status_label(),
                cwd,
                id_width = id_width,
                status_width = status_width
            ));
        }
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
            host: None,
            shut_down: false,
            idle: false,
            archived,
        }
    }

    fn info(status: &str, updated: &str) -> infinity::SessionInfo {
        infinity::SessionInfo {
            last_updated: updated.to_owned(),
            host: None,
            status: status.to_owned(),
        }
    }

    fn remote(name: &str, args: &[&str]) -> infinity::Remote {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","ssh_args":{}}}"#,
            serde_json::to_string(args).expect("args")
        ))
        .expect("remote parses")
    }

    /// remotes.json does not RENAME anything -- `name` is already the label
    /// Infinity prefixes onto the id. What it adds is the machine behind that
    /// label, which the id cannot tell you.
    #[test]
    fn shows_the_ssh_target_behind_the_label() {
        let sessions = vec![Session {
            id: "ada/abc".to_owned(),
            cwd: PathBuf::from("/home/u/p"),
            host: Some("ada".to_owned()),
            status: Some("Running".to_owned()),
            last_updated: None,
            archived: false,
        }];
        let mut remotes = HashMap::new();
        remotes.insert("ada".to_owned(), remote("ada", &["ada.example.com"]));
        let table = render_table_with_remotes(&sessions, &remotes);
        assert!(table.contains("ada (ada.example.com)"), "{table}");
    }

    /// An unconfigured label still prints, on its own. A session can name a
    /// remote that is no longer in remotes.json.
    #[test]
    fn an_unconfigured_label_stands_alone() {
        let sessions = vec![Session {
            id: "gone/abc".to_owned(),
            cwd: PathBuf::from("/p"),
            host: Some("gone".to_owned()),
            status: None,
            last_updated: None,
            archived: false,
        }];
        let table = render_table_with_remotes(&sessions, &HashMap::new());
        assert!(table.contains("gone"), "{table}");
        assert!(!table.contains("("), "no parens without a target: {table}");
    }

    /// Flags are not hostnames. `-i key` and friends must not be printed as
    /// the machine; the value after a value-taking option is skipped too.
    #[test]
    fn ssh_target_skips_flags_and_their_values() {
        assert_eq!(
            remote("r", &["-i", "/k/id_ed25519", "host.example"]).ssh_target(),
            Some("host.example")
        );
        assert_eq!(
            remote("r", &["-J", "jump.example", "target.example"]).ssh_target(),
            Some("target.example")
        );
        assert_eq!(remote("r", &["user@host"]).ssh_target(), Some("user@host"));
        // All flags and no target: the label stands alone rather than showing
        // a flag as if it were a machine.
        assert_eq!(remote("r", &["-v"]).ssh_target(), None);
        assert_eq!(remote("r", &[]).ssh_target(), None);
    }

    /// A label equal to its target is not worth repeating.
    #[test]
    fn does_not_repeat_a_label_that_equals_its_target() {
        let sessions = vec![Session {
            id: "h/abc".to_owned(),
            cwd: PathBuf::from("/p"),
            host: Some("h".to_owned()),
            status: None,
            last_updated: None,
            archived: false,
        }];
        let mut remotes = HashMap::new();
        remotes.insert("h".to_owned(), remote("h", &["h"]));
        let table = render_table_with_remotes(&sessions, &remotes);
        assert!(!table.contains("h (h)"), "{table}");
    }

    /// Infinity shows a remote session as `devbox/def456`, so the host is
    /// already in the id. herdr was dropping it and printing the remote's cwd
    /// as if it were local.
    #[test]
    fn derives_the_host_from_infinitys_remote_id_prefix() {
        let mut store = HashMap::new();
        store.insert("ada/abc123".to_owned(), entry("/home/u/remote-proj", false));
        let sessions = collect(&store, &HashMap::new());
        assert_eq!(sessions[0].host.as_deref(), Some("ada"));
        // And it shows up in the pane beside that host's directory.
        let table = render_table(&sessions);
        assert!(table.contains("HOST"), "{table}");
        assert!(table.contains("ada"), "{table}");
        assert!(table.contains("/home/u/remote-proj"), "{table}");
    }

    /// A local session has no prefix and must not acquire a host.
    #[test]
    fn a_local_id_yields_no_host() {
        let mut store = HashMap::new();
        store.insert("abc123".to_owned(), entry("/work", false));
        let sessions = collect(&store, &HashMap::new());
        assert_eq!(sessions[0].host, None);
    }

    /// An explicit host beats the prefix: the prefix is a fallback, not the
    /// truth, and a session that migrated has the right answer elsewhere.
    #[test]
    fn an_explicit_host_beats_the_id_prefix() {
        let mut store = HashMap::new();
        let mut e = entry("/work", false);
        e.host = Some("real-host".to_owned());
        store.insert("stale-prefix/abc".to_owned(), e);
        let sessions = collect(&store, &HashMap::new());
        assert_eq!(sessions[0].host.as_deref(), Some("real-host"));
    }

    /// Do not invent a host from an id that merely contains a slash. Only a
    /// single leading segment is Infinity's remote convention.
    #[test]
    fn does_not_invent_a_host_from_a_multi_slash_id() {
        assert_eq!(split_remote_id("a/b/c"), None);
        assert_eq!(split_remote_id("/leading"), None);
        assert_eq!(split_remote_id("trailing/"), None);
        assert_eq!(split_remote_id("plain"), None);
        assert_eq!(
            split_remote_id("ada/abc"),
            Some(("ada".to_owned(), "abc".to_owned()))
        );
    }

    /// A remote session must be labelled with the host its directory is on.
    /// Without this the pane printed `ada`'s cwd as if it were local, and a
    /// path that also exists here was indistinguishable from a local folder.
    #[test]
    fn renders_a_host_column_when_a_session_reports_one() {
        let sessions = vec![
            Session {
                id: "local-1".to_owned(),
                cwd: PathBuf::from("/home/u/proj"),
                host: None,
                status: Some("Idle".to_owned()),
                last_updated: None,
                archived: false,
            },
            Session {
                id: "ada-1".to_owned(),
                cwd: PathBuf::from("/home/u/remote-proj"),
                host: Some("ada".to_owned()),
                status: Some("Running".to_owned()),
                last_updated: None,
                archived: false,
            },
        ];
        let table = render_table(&sessions);
        assert!(table.contains("HOST"), "no HOST header: {table}");
        assert!(table.contains("ada"), "host not shown: {table}");
        // The remote row carries BOTH its host and that host's directory.
        let ada = table
            .lines()
            .find(|l| l.starts_with("ada-1"))
            .expect("no ada row");
        assert!(ada.contains("ada"), "{ada}");
        assert!(ada.contains("/home/u/remote-proj"), "{ada}");
        // Unreported reads as "-", never as the local hostname: inventing
        // "local" here is the confidently-wrong label this avoids.
        let local = table
            .lines()
            .find(|l| l.starts_with("local-1"))
            .expect("no local row");
        assert!(local.contains(" - "), "unreported host not '-': {local}");
    }

    /// With no remote sessions the table keeps its original three columns, so
    /// the common all-local listing is not widened by a column of dashes.
    #[test]
    fn omits_the_host_column_when_nothing_reports_a_host() {
        let sessions = vec![Session {
            id: "only".to_owned(),
            cwd: PathBuf::from("/work"),
            host: None,
            status: Some("Idle".to_owned()),
            last_updated: None,
            archived: false,
        }];
        let table = render_table(&sessions);
        assert!(!table.contains("HOST"), "HOST shown with no hosts: {table}");
        assert!(table.contains("DIRECTORY"), "{table}");
    }

    /// The store records a host too, for a session the daemon is not
    /// reporting (on disk but not live).
    #[test]
    fn takes_the_host_from_the_store_when_the_daemon_is_silent() {
        let mut store = HashMap::new();
        let mut e = entry("/home/u/p", false);
        e.host = Some("ada".to_owned());
        store.insert("s1".to_owned(), e);
        let sessions = collect(&store, &HashMap::new());
        assert_eq!(sessions[0].host.as_deref(), Some("ada"));
    }

    /// A session that migrated hosts has a stale host on disk until it
    /// flushes, so the daemon's live value wins.
    #[test]
    fn daemon_host_overrides_a_stale_store_host() {
        let mut store = HashMap::new();
        let mut e = entry("/home/u/p", false);
        e.host = Some("old-host".to_owned());
        store.insert("s1".to_owned(), e);
        let mut daemon = HashMap::new();
        let mut i = info("Running", "2026-11-08T00:00:00Z");
        i.host = Some("ada".to_owned());
        daemon.insert("s1".to_owned(), i);
        let sessions = collect(&store, &daemon);
        assert_eq!(sessions[0].host.as_deref(), Some("ada"));
    }

    /// An existing store has no host key at all; it must still parse, and
    /// must not be reported as some guessed host.
    #[test]
    fn a_store_without_a_host_key_parses_as_unreported() {
        let e: SessionEntry =
            serde_json::from_str(r#"{"cwd":"/work","archived":false}"#).expect("parses");
        assert_eq!(e.host, None);
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
                host: None,
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "abc-2".to_owned(),
                cwd: PathBuf::from("/work"),
                host: None,
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "xyz".to_owned(),
                cwd: PathBuf::from("/work"),
                host: None,
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
                host: None,
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "elsewhere".to_owned(),
                cwd: PathBuf::from("/other"),
                host: None,
                status: None,
                last_updated: None,
                archived: false,
            },
            Session {
                id: "unknown".to_owned(),
                cwd: PathBuf::new(),
                host: None,
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
            host: None,
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
