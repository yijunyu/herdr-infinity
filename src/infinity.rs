//! Observation of the Infinity daemon.
//!
//! Infinity Code keeps every conversation in a daemon (`~/.infinity/daemon.sock`)
//! and broadcasts a `SessionsUpdated` message whenever a session changes state.
//! We connect as a passive client — never sending a `ClientMessage` — and read
//! those broadcasts. The wire format is a 4-byte big-endian length prefix
//! followed by a JSON-serialized `DaemonMessage` (externally tagged enum).
//!
//! Session attribution is the only heuristic here: the daemon does not tell a
//! passive observer which client is attached to which session. We use, in
//! order, an explicit `--session` argument, sessions created after the wrapper
//! started in the pane's directory, and finally the most recently updated
//! session in that directory. `~/.infinity/sessions.json` supplies each
//! session's working directory.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::herdr::{self, AgentState, Reporter};

const DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_FRAME: usize = 64 * 1024 * 1024;
const FOLLOW_EXIT_GRACE: Duration = Duration::from_secs(10);
const FOLLOW_MAX: Duration = Duration::from_secs(6 * 3600);

#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    #[serde(default)]
    pub last_updated: String,
    /// The host the session actually runs on, when the daemon says so.
    /// `None` means "not reported", which is NOT the same as "local": the
    /// daemon protocol carries no host field today, so every live session
    /// reads as unreported until it does. The pane must not print a host it
    /// was not told, because a directory labelled with the wrong machine is
    /// worse than one labelled with none.
    #[serde(default)]
    pub host: Option<String>,
    /// `Running`, `Idle`, `Stopped`, `WaitingForChoice`, `Migrating`, or
    /// `Archived` today; kept as a string so a future status never breaks the
    /// bridge.
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionEntry {
    pub cwd: PathBuf,
    /// The host `cwd` is a path ON. Absent in every store written so far, so
    /// it defaults to `None` and existing files keep parsing unchanged.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub shut_down: bool,
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Deserialize)]
struct StoreFile {
    #[serde(default)]
    sessions: HashMap<String, SessionEntry>,
}

/// Read `~/.infinity/sessions.json`, which records each session's cwd and
/// lifecycle flags. Missing or malformed files are treated as empty.
pub fn read_store_sessions() -> HashMap<String, SessionEntry> {
    let path = crate::home_dir().join(".infinity").join("sessions.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str::<StoreFile>(&contents)
        .map(|file| file.sessions)
        .unwrap_or_default()
}

/// Extract `-s`/`--session` from the arguments passed through to Infinity.
pub fn session_prefix_from_args(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-s" || arg == "--session" {
            return args.get(index + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix("--session=") {
            return Some(value.to_owned());
        }
        if let Some(value) = arg.strip_prefix("-s") {
            if !value.is_empty() && !arg.starts_with("--") {
                return Some(value.to_owned());
            }
        }
        index += 1;
    }
    None
}

pub struct BridgeOptions {
    /// `--session` prefix supplied by the user, if any.
    pub explicit: Option<String>,
    /// The directory this pane is running in.
    pub pane_cwd: PathBuf,
    /// Sessions known before the wrapper started.
    pub baseline: HashMap<String, SessionEntry>,
    /// Keep observing after the child exits (`infinity -H`).
    pub follow_after_exit: bool,
    /// `--local` runs the agent in-process; there is no daemon to observe.
    pub local: bool,
}

struct Bridge {
    options: BridgeOptions,
    sessions: HashMap<String, SessionInfo>,
    store: HashMap<String, SessionEntry>,
    target: Option<String>,
    last_report: Option<(AgentState, Option<String>)>,
    store_refreshed: Option<Instant>,
}

impl Bridge {
    fn new(options: BridgeOptions) -> Self {
        Self {
            options,
            sessions: HashMap::new(),
            store: HashMap::new(),
            target: None,
            last_report: None,
            store_refreshed: None,
        }
    }

    fn on_frame(&mut self, frame: &[u8], reporter: &Reporter) {
        let Ok(value) = serde_json::from_slice::<Value>(frame) else {
            return;
        };
        let Some(object) = value.as_object() else {
            return;
        };

        if let Some(welcome) = object.get("Welcome") {
            if let Some(sessions) = welcome.get("sessions") {
                self.sessions = parse_sessions(sessions);
            }
            self.refresh_store(true);
            self.resolve_and_report(reporter);
        } else if let Some(update) = object.get("SessionsUpdated") {
            if let Some(sessions) = update.get("sessions") {
                for (id, info) in parse_sessions(sessions) {
                    self.sessions.insert(id, info);
                }
            }
            self.resolve_and_report(reporter);
        }
    }

    fn resolve_and_report(&mut self, reporter: &Reporter) {
        let Some(target) = self.resolve_target() else {
            // Claim the pane so Herdr shows it as an agent even before the
            // user has picked or created a session.
            if self.last_report.is_none() {
                self.last_report = Some((AgentState::Idle, None));
                reporter.report(AgentState::Idle, None, None, false);
            }
            return;
        };

        let Some(info) = self.sessions.get(&target).cloned() else {
            return;
        };
        let Some(state) = state_for(&info.status) else {
            return;
        };

        if self.last_report == Some((state, Some(target.clone()))) {
            return;
        }

        let message = (state == AgentState::Blocked).then_some("waiting for user choice");
        self.last_report = Some((state, Some(target.clone())));
        reporter.report(state, message, Some(&target), true);
    }

    fn resolve_target(&mut self) -> Option<String> {
        // Once a session is chosen, keep it until it is archived or removed.
        if let Some(target) = self.target.clone() {
            match self.sessions.get(&target) {
                Some(info) if info.status != "Archived" => return Some(target),
                _ => self.target = None,
            }
        }

        if let Some(prefix) = self.options.explicit.clone() {
            let matches: Vec<String> = self
                .sessions
                .iter()
                .filter(|(id, info)| id.starts_with(&prefix) && info.status != "Archived")
                .map(|(id, _)| id.clone())
                .collect();
            if let Some(id) = self.most_recent(&matches) {
                self.target = Some(id.clone());
                return Some(id);
            }
            return None;
        }

        self.refresh_store(false);

        // A session created after we started, in this pane's directory.
        let created: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, info)| {
                info.status != "Archived"
                    && !self.options.baseline.contains_key(*id)
                    && self.cwd_matches(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if let Some(id) = self.most_recent(&created) {
            self.target = Some(id.clone());
            return Some(id);
        }

        // Any live session in this pane's directory.
        let in_cwd: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, info)| info.status != "Archived" && self.cwd_matches(id))
            .map(|(id, _)| id.clone())
            .collect();
        if let Some(id) = self.most_recent(&in_cwd) {
            self.target = Some(id.clone());
            return Some(id);
        }

        // Last resort: a single Running session that did not exist before we
        // started (the store entry may not have been flushed yet).
        let running: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, info)| {
                info.status == "Running" && !self.options.baseline.contains_key(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if running.len() == 1 {
            self.target = Some(running[0].clone());
            return Some(running[0].clone());
        }

        None
    }

    fn most_recent(&self, ids: &[String]) -> Option<String> {
        ids.iter()
            .max_by(|left, right| {
                let left_stamp = self
                    .sessions
                    .get(*left)
                    .map(|info| info.last_updated.as_str())
                    .unwrap_or("");
                let right_stamp = self
                    .sessions
                    .get(*right)
                    .map(|info| info.last_updated.as_str())
                    .unwrap_or("");
                left_stamp.cmp(right_stamp).then_with(|| left.cmp(right))
            })
            .cloned()
    }

    fn cwd_matches(&self, id: &str) -> bool {
        self.store
            .get(id)
            .map(|entry| same_dir(&entry.cwd, &self.options.pane_cwd))
            .unwrap_or(false)
    }

    fn refresh_store(&mut self, force: bool) {
        if !force
            && self
                .store_refreshed
                .map(|at| at.elapsed() < Duration::from_millis(500))
                .unwrap_or(false)
        {
            return;
        }
        self.store = read_store_sessions();
        self.store_refreshed = Some(Instant::now());
    }

    fn target_is_idle(&self) -> bool {
        self.target
            .as_ref()
            .and_then(|target| self.sessions.get(target))
            .map(|info| matches!(info.status.as_str(), "Idle" | "Stopped" | "Archived"))
            .unwrap_or(true)
    }
}

fn state_for(status: &str) -> Option<AgentState> {
    match status {
        "Running" | "Migrating" => Some(AgentState::Working),
        "Idle" | "Stopped" => Some(AgentState::Idle),
        "WaitingForChoice" => Some(AgentState::Blocked),
        _ => None,
    }
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

fn parse_sessions(value: &Value) -> HashMap<String, SessionInfo> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Length-delimited frame reader that tolerates read timeouts without losing
/// partially received bytes.
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    pub fn poll(&mut self, stream: &mut UnixStream) -> io::Result<Vec<Vec<u8>>> {
        let mut chunk = [0u8; 64 * 1024];
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the connection",
                ));
            }
            Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(error),
        }

        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let length = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            if length > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeds the maximum size",
                ));
            }
            if self.buffer.len() < 4 + length {
                break;
            }
            frames.push(self.buffer[4..4 + length].to_vec());
            self.buffer.drain(..4 + length);
        }
        Ok(frames)
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Ask the daemon for its live session list and disconnect.
///
/// The daemon sends `Welcome` to every client that connects, so a passive
/// observer can learn the live statuses without ever sending a message. Used by
/// the management surface, which runs as a short-lived command rather than for
/// the lifetime of a pane. Returns `None` when the daemon is not reachable
/// within `timeout`; callers degrade to the on-disk store.
pub fn probe_daemon_sessions(timeout: Duration) -> Option<HashMap<String, SessionInfo>> {
    let socket = crate::home_dir().join(".infinity").join("daemon.sock");
    let deadline = Instant::now() + timeout;

    let mut stream = UnixStream::connect(&socket).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;

    let mut reader = FrameReader::new();
    while Instant::now() < deadline {
        let frames = match reader.poll(&mut stream) {
            Ok(frames) => frames,
            Err(error) if is_timeout(&error) => continue,
            Err(_) => return None,
        };
        for frame in frames {
            let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            // `Welcome` carries the full list; `SessionsUpdated` may arrive
            // first if a session changes while we are connecting.
            let sessions = value
                .get("Welcome")
                .or_else(|| value.get("SessionsUpdated"))
                .and_then(|message| message.get("sessions"));
            if let Some(sessions) = sessions {
                return Some(parse_sessions(sessions));
            }
        }
    }
    None
}

/// Run the bridge until the child exits (and, for `-H`, until its session
/// goes idle). Returns the child's exit status.
pub fn bridge(
    options: BridgeOptions,
    reporter: &Reporter,
    child: &mut Child,
    stop: &AtomicBool,
) -> Option<ExitStatus> {
    if options.local {
        return child.wait().ok();
    }

    let mut bridge = Bridge::new(options);
    let socket = crate::home_dir().join(".infinity").join("daemon.sock");

    let mut stream: Option<UnixStream> = None;
    let mut reader = FrameReader::new();
    let mut child_status: Option<ExitStatus> = None;
    let mut connect_deadline = Instant::now() + DAEMON_CONNECT_TIMEOUT;
    let mut last_connect = Instant::now() - Duration::from_secs(1);
    let mut exit_grace: Option<Instant> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        if child_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    child_status = Some(status);
                    if !bridge.options.follow_after_exit {
                        break;
                    }
                    exit_grace = Some(Instant::now());
                }
                Ok(None) => {}
                Err(error) => {
                    herdr::log(&format!("failed to poll infinity process: {error}"));
                    child_status = child.wait().ok();
                    break;
                }
            }
        }

        if stream.is_none()
            && Instant::now() < connect_deadline
            && last_connect.elapsed() >= Duration::from_millis(100)
        {
            last_connect = Instant::now();
            if let Ok(connection) = UnixStream::connect(&socket) {
                let _ = connection.set_read_timeout(Some(READ_TIMEOUT));
                reader.clear();
                stream = Some(connection);
                herdr::log("connected to the Infinity daemon");
            }
        }

        match stream.as_mut() {
            Some(connection) => match reader.poll(connection) {
                Ok(frames) => {
                    for frame in frames {
                        bridge.on_frame(&frame, reporter);
                    }
                }
                Err(error) if is_timeout(&error) => {}
                Err(error) => {
                    herdr::log(&format!("Infinity daemon connection closed: {error}"));
                    stream = None;
                    connect_deadline = Instant::now() + DAEMON_CONNECT_TIMEOUT;
                }
            },
            None => std::thread::sleep(Duration::from_millis(50)),
        }

        if let Some(since) = exit_grace {
            if bridge.target_is_idle() || since.elapsed() > FOLLOW_MAX {
                break;
            }
            if bridge.target.is_none() && since.elapsed() > FOLLOW_EXIT_GRACE {
                break;
            }
        }
    }

    if child_status.is_none() {
        child_status = child.wait().ok();
    }
    child_status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(json: &str) -> HashMap<String, SessionInfo> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn session_status_maps_to_herdr_states() {
        assert_eq!(state_for("Running"), Some(AgentState::Working));
        assert_eq!(state_for("Migrating"), Some(AgentState::Working));
        assert_eq!(state_for("Idle"), Some(AgentState::Idle));
        assert_eq!(state_for("Stopped"), Some(AgentState::Idle));
        assert_eq!(state_for("WaitingForChoice"), Some(AgentState::Blocked));
        assert_eq!(state_for("Archived"), None);
        assert_eq!(state_for("SomeFutureState"), None);
    }

    #[test]
    fn parses_welcome_and_updates() {
        let mut bridge = Bridge::new(BridgeOptions {
            explicit: None,
            pane_cwd: PathBuf::from("/work/project"),
            baseline: HashMap::new(),
            follow_after_exit: false,
            local: false,
        });

        let frame = br#"{"Welcome":{"sessions":{"abc":{"title":"one","last_updated":"2026-10-28T10:00:00Z","total_tokens_used":3,"status":"Idle"}},"available_models":[],"default_model_name":"m","default_context_window":1,"provider_name":"p"}}"#;
        bridge.on_frame(frame, &Reporter::new());
        assert_eq!(
            bridge.sessions.get("abc").map(|info| info.status.as_str()),
            Some("Idle")
        );

        let frame = br#"{"SessionsUpdated":{"sessions":{"abc":{"title":"one","last_updated":"2026-10-28T10:00:01Z","total_tokens_used":4,"status":"Running"}}}}"#;
        bridge.on_frame(frame, &Reporter::new());
        assert_eq!(
            bridge.sessions.get("abc").map(|info| info.status.as_str()),
            Some("Running")
        );
    }

    #[test]
    fn explicit_prefix_wins_and_must_be_unique() {
        let mut bridge = Bridge::new(BridgeOptions {
            explicit: Some("abc".to_owned()),
            pane_cwd: PathBuf::from("/elsewhere"),
            baseline: HashMap::new(),
            follow_after_exit: false,
            local: false,
        });
        bridge.sessions = sessions(
            r#"{"abc-1":{"last_updated":"2026-10-28T10:00:00Z","status":"Idle"},
                "abc-2":{"last_updated":"2026-10-28T11:00:00Z","status":"Idle"}}"#,
        );
        assert_eq!(bridge.resolve_target(), Some("abc-2".to_owned()));
    }

    #[test]
    fn new_session_in_the_pane_directory_wins() {
        let mut baseline = HashMap::new();
        baseline.insert(
            "old".to_owned(),
            SessionEntry {
                cwd: PathBuf::from("/work/project"),
                host: None,
                shut_down: false,
                idle: true,
                archived: false,
            },
        );
        let mut bridge = Bridge::new(BridgeOptions {
            explicit: None,
            pane_cwd: PathBuf::from("/work/project"),
            baseline,
            follow_after_exit: false,
            local: false,
        });
        bridge.store.insert(
            "old".to_owned(),
            SessionEntry {
                cwd: PathBuf::from("/work/project"),
                host: None,
                shut_down: false,
                idle: true,
                archived: false,
            },
        );
        bridge.store.insert(
            "new".to_owned(),
            SessionEntry {
                cwd: PathBuf::from("/work/project"),
                host: None,
                shut_down: false,
                idle: false,
                archived: false,
            },
        );
        bridge.sessions = sessions(
            r#"{"old":{"last_updated":"2026-10-28T10:00:00Z","status":"Idle"},
                "new":{"last_updated":"2026-10-28T12:00:00Z","status":"Running"}}"#,
        );
        bridge.store_refreshed = Some(Instant::now());
        assert_eq!(bridge.resolve_target(), Some("new".to_owned()));
    }

    #[test]
    fn session_prefix_parsing() {
        let args = ["-s", "abc"].map(str::to_owned);
        assert_eq!(session_prefix_from_args(&args), Some("abc".to_owned()));
        let args = ["--session=xyz"].map(str::to_owned);
        assert_eq!(session_prefix_from_args(&args), Some("xyz".to_owned()));
        let args = ["-sabc"].map(str::to_owned);
        assert_eq!(session_prefix_from_args(&args), Some("abc".to_owned()));
        let args = ["-i", "hello"].map(str::to_owned);
        assert_eq!(session_prefix_from_args(&args), None);
    }

    #[test]
    fn frame_reader_handles_split_and_multiple_frames() {
        use std::io::Write;
        use std::os::unix::net::UnixListener;

        let dir =
            std::env::temp_dir().join(format!("herdr-infinity-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let payload_a = br#"{"SessionsUpdated":{"sessions":{}}}"#;
        let payload_b = br#"{"Welcome":{"sessions":{}}}"#;
        let mut wire = Vec::new();
        for payload in [payload_a.as_slice(), payload_b.as_slice()] {
            wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            wire.extend_from_slice(payload);
        }

        let writer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(&wire[..3]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            stream.write_all(&wire[3..]).unwrap();
        });

        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut reader = FrameReader::new();
        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while frames.len() < 2 && Instant::now() < deadline {
            match reader.poll(&mut client) {
                Ok(mut parsed) => frames.append(&mut parsed),
                Err(error) if is_timeout(&error) => {}
                Err(error) => panic!("read failed: {error}"),
            }
        }
        writer.join().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], payload_a);
        assert_eq!(frames[1], payload_b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
