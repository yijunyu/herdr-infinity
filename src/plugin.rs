//! The Herdr plugin surface.
//!
//! Herdr invokes a plugin as a plain argv command with context in the
//! environment (`HERDR_BIN_PATH`, `HERDR_PANE_ID`, `HERDR_PLUGIN_*`). Actions
//! and pane entrypoints are one-shot processes, so none of them can hold the
//! daemon subscription that state reporting needs — that stays in
//! `herdr-infinity run`. This module implements the management half: list the
//! sessions, and open one in a pane.
//!
//! Opening a session is done through the Herdr CLI (`pane split`, `pane run`)
//! rather than by spawning Infinity here, so the new pane is a first-class
//! Herdr pane that reports its own state.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::sessions::{self, Session};

/// Context Herdr injects into every plugin invocation.
pub struct Context {
    pub herdr_bin: PathBuf,
    pub pane_id: Option<String>,
    pub cwd: PathBuf,
}

impl Context {
    pub fn from_env() -> Self {
        let herdr_bin = std::env::var_os("HERDR_BIN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("herdr"));
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            herdr_bin,
            pane_id,
            cwd,
        }
    }

    fn herdr(&self) -> Command {
        let mut cmd = Command::new(&self.herdr_bin);
        cmd.stdin(Stdio::null());
        cmd
    }
}

/// Run a Herdr CLI command and capture stdout.
///
/// Herdr reports server errors as JSON on stderr with exit status 1, so a
/// non-zero status carries a message worth surfacing to the user.
fn capture(mut cmd: Command) -> Result<String, String> {
    let output = cmd
        .output()
        .map_err(|error| format!("failed to run the Herdr CLI: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("Herdr CLI exited with status {}", output.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Pull a string field out of a Herdr JSON response without a full schema.
fn json_field(body: &str, path: &[&str]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut cursor = &value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(str::to_owned)
}

/// Split a sibling pane and run a command in it.
///
/// Follows Herdr's own guidance for background work: preserve the caller's
/// directory and leave the user's focus where it was.
fn open_in_new_pane(ctx: &Context, command: &str, focus: bool) -> Result<String, String> {
    let mut split = ctx.herdr();
    split.arg("pane").arg("split");
    match &ctx.pane_id {
        Some(pane) => {
            split.arg("--pane").arg(pane);
        }
        None => {
            split.arg("--current");
        }
    }
    split
        .arg("--direction")
        .arg("right")
        .arg("--cwd")
        .arg(&ctx.cwd);
    if !focus {
        split.arg("--no-focus");
    }

    let body = capture(split)?;
    let pane_id = json_field(&body, &["result", "pane", "pane_id"])
        .ok_or_else(|| "Herdr did not report a new pane id".to_owned())?;

    let mut run = ctx.herdr();
    run.arg("pane").arg("run").arg(&pane_id).arg(command);
    capture(run)?;

    Ok(pane_id)
}

/// `sessions` — list every Infinity session.
pub fn list_sessions(all: bool, here: Option<&PathBuf>) -> String {
    let mut sessions = sessions::load();
    if let Some(dir) = here {
        sessions = sessions::in_directory(&sessions, dir);
    }
    if !all {
        sessions.retain(Session::is_live);
    }
    sessions::render_table(&sessions)
}

/// `open <prefix>` — open an existing session in a new pane.
pub fn open_session(ctx: &Context, prefix: &str, focus: bool) -> Result<String, String> {
    let sessions = sessions::load();
    let session = sessions::resolve_prefix(&sessions, prefix)?;
    if !session.is_live() {
        return Err(format!(
            "session {} is archived; Infinity cannot resume it",
            session.id
        ));
    }
    let command = sessions::open_command(Some(&session.id));
    let pane = open_in_new_pane(ctx, &command, focus)?;
    Ok(format!("opened session {} in pane {pane}", session.id))
}

/// `new` — start a fresh Infinity session in a new pane.
pub fn new_session(ctx: &Context, focus: bool) -> Result<String, String> {
    let command = sessions::open_command(None);
    let pane = open_in_new_pane(ctx, &command, focus)?;
    Ok(format!("started a new Infinity session in pane {pane}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_pane_id_from_a_split_response() {
        let body = r#"{"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p2"}}}"#;
        assert_eq!(
            json_field(body, &["result", "pane", "pane_id"]),
            Some("w1:p2".to_owned())
        );
    }

    #[test]
    fn missing_json_fields_are_none_not_a_panic() {
        assert_eq!(json_field("{}", &["result", "pane", "pane_id"]), None);
        assert_eq!(json_field("not json", &["result"]), None);
        // A non-string leaf is not a pane id.
        assert_eq!(
            json_field(r#"{"result":{"pane":7}}"#, &["result", "pane"]),
            None
        );
    }

    #[test]
    fn context_defaults_to_the_herdr_on_path() {
        // HERDR_BIN_PATH is absent in the test environment.
        let ctx = Context::from_env();
        assert!(ctx
            .herdr_bin
            .as_os_str()
            .to_string_lossy()
            .contains("herdr"));
    }
}
