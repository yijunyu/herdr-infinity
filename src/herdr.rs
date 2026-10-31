//! The Herdr side of the integration.
//!
//! Herdr's documented third-party integration surface is intentionally small:
//! a process running in a Herdr pane calls back into the Herdr CLI with
//! `pane report-agent`, `pane report-agent-session`, and `pane release-agent`.
//! Reports are best-effort and must never slow the agent down, so everything
//! here runs on a detached worker thread with a short timeout.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Which agent a reporter speaks for.
///
/// Herdr keys a pane claim on the agent label, and the resume command differs
/// per agent, so both travel together. The source id is what Herdr attributes
/// the report to; any value is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentKind {
    /// Herdr's agent label, e.g. `infinity` or `letta`.
    pub label: &'static str,
    /// The `--source` id Herdr records the report under.
    pub source: &'static str,
}

impl AgentKind {
    pub const INFINITY: Self = Self {
        label: "infinity",
        source: "herdr-infinity",
    };
    pub const LETTA: Self = Self {
        label: "letta",
        source: "herdr-letta",
    };

    /// The argv that reopens a session in a fresh pane, so a Herdr restart
    /// returns to the same conversation.
    fn resume(&self, session: &str) -> Vec<String> {
        match self.label {
            "letta" => vec!["letta".to_owned(), "--agent".to_owned(), session.to_owned()],
            // Infinity is reopened through the wrapper so the new pane keeps
            // reporting its state.
            _ => vec![
                "herdr-infinity".to_owned(),
                "run".to_owned(),
                "--".to_owned(),
                "infinity".to_owned(),
                "-s".to_owned(),
                session.to_owned(),
            ],
        }
    }
}

/// A state Herdr understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// True when this process is running inside a Herdr pane with the environment
/// Herdr promises to inject.
pub fn active() -> bool {
    std::env::var_os("HERDR_ENV").as_deref() == Some(std::ffi::OsStr::new("1"))
        && std::env::var_os("HERDR_PANE_ID").is_some()
        && std::env::var_os("HERDR_BIN_PATH").is_some()
}

pub struct Reporter {
    tx: Option<Sender<Job>>,
}

enum Job {
    Report {
        state: AgentState,
        message: Option<String>,
        session: Option<String>,
        resume: bool,
    },
    Release,
    Sync(Sender<()>),
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    /// A reporter for Infinity, the original caller.
    pub fn new() -> Self {
        Self::for_agent(AgentKind::INFINITY)
    }

    pub fn for_agent(kind: AgentKind) -> Self {
        if !active() {
            return Self { tx: None };
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || worker(rx, kind));
        Self { tx: Some(tx) }
    }

    pub fn report(
        &self,
        state: AgentState,
        message: Option<&str>,
        session: Option<&str>,
        resume: bool,
    ) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Report {
                state,
                message: message.map(str::to_owned),
                session: session.map(str::to_owned),
                resume,
            });
        }
    }

    pub fn release(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Release);
        }
    }

    /// Wait until the worker has drained everything queued so far, up to
    /// `timeout`. Used just before the wrapper exits so the final release
    /// report is not lost.
    pub fn flush(&self, timeout: Duration) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (ack_tx, ack_rx) = channel();
        if tx.send(Job::Sync(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }
}

fn worker(rx: Receiver<Job>, kind: AgentKind) {
    let mut last_seq = now_nanos();
    let mut pending: Option<Job> = None;

    loop {
        let mut command = match pending.take() {
            Some(command) => command,
            None => match rx.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };

        // Collapse a burst of state reports to the newest one, but never
        // reorder a Release or a Sync behind a later report.
        if matches!(command, Job::Report { .. }) {
            loop {
                match rx.try_recv() {
                    Ok(Job::Report {
                        state,
                        message,
                        session,
                        resume,
                    }) => {
                        command = Job::Report {
                            state,
                            message,
                            session,
                            resume,
                        };
                    }
                    Ok(other) => {
                        pending = Some(other);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }

        match command {
            Job::Report {
                state,
                message,
                session,
                resume,
            } => {
                last_seq = now_nanos().max(last_seq.saturating_add(1));
                let mut cmd = base_command(kind, false, last_seq);
                cmd.arg("--state").arg(state.as_str());
                if let Some(message) = message {
                    cmd.arg("--message").arg(message);
                }
                if let Some(session) = &session {
                    cmd.arg("--agent-session-id").arg(session);
                }
                if resume {
                    if let Some(session) = &session {
                        cmd.arg("--").args(kind.resume(session));
                    }
                }
                run_with_timeout(cmd);
            }
            Job::Release => {
                last_seq = now_nanos().max(last_seq.saturating_add(1));
                run_with_timeout(base_command(kind, true, last_seq));
            }
            Job::Sync(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

fn base_command(kind: AgentKind, release: bool, seq: u64) -> Command {
    let bin = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let pane = std::env::var_os("HERDR_PANE_ID").unwrap_or_default();
    let mut cmd = Command::new(bin);
    cmd.arg("pane")
        .arg(if release {
            "release-agent"
        } else {
            "report-agent"
        })
        .arg(pane)
        .arg("--source")
        .arg(kind.source)
        .arg("--agent")
        .arg(kind.label)
        .arg("--seq")
        .arg(seq.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn run_with_timeout(mut cmd: Command) {
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            log(&format!("failed to start Herdr CLI: {error}"));
            return;
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if started.elapsed() > Duration::from_secs(2) {
                    let _ = child.kill();
                    let _ = child.wait();
                    log("Herdr CLI report timed out");
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                log(&format!("failed to wait for Herdr CLI: {error}"));
                return;
            }
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

/// Best-effort debug log. The terminal belongs to Infinity's TUI, so we never
/// write to stdout/stderr from the worker.
pub fn log(message: &str) {
    if std::env::var_os("HERDR_INFINITY_DEBUG").is_none() {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn log_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("herdr-infinity").join("herdr-infinity.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_strings_match_herdr_vocabulary() {
        assert_eq!(AgentState::Idle.as_str(), "idle");
        assert_eq!(AgentState::Working.as_str(), "working");
        assert_eq!(AgentState::Blocked.as_str(), "blocked");
    }
}
