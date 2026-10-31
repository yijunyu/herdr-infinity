//! `herdr-letta` — run Letta Code in a Herdr pane and report its state.
//!
//! Herdr 0.9.1 ships `letta` as an agent kind but does not recognise a running
//! Letta Code pane: it sees no process command, only a terminal title Letta
//! sets to the agent's own name, over a `node` process. So the pane shows
//! `agent: None, status: unknown` and never reaches the sidebar.
//!
//! Herdr also refuses an external report for a pane whose foreground
//! application it is already tracking, which rules out an observer poking at
//! the pane from outside. What it does accept is a report from the process
//! that *owns* the pane — the same mechanism `herdr-infinity run` uses.
//!
//! This wrapper is therefore the pane's foreground process: it spawns Letta,
//! polls the local backend's conversation log, and reports
//! `working`/`idle` until Letta exits. Outside Herdr it `exec`s Letta
//! directly, so it is safe to keep on `PATH`.
//!
//! Compared with the Infinity bridge this is a weaker observer, and
//! deliberately so. Infinity broadcasts status on a daemon socket; Letta has
//! no socket, so state is inferred from the shape of the log and `blocked` is
//! never reported — Herdr's own screen detection is what recognises an
//! approval prompt, and it cannot see Letta at all.
//!
//! ## What this achieves, and what it does not (Herdr 0.9.1)
//!
//! Verified: the wrapper **does** put Letta in the sidebar. The pane shows
//! `letta idle`, and the integration hook's session id rides along, so
//! `herdr agent list` reports the agent and its conversation.
//!
//! Not achieved: **live state transitions**. The initial claim lands because
//! it is reported before Letta's TUI takes the foreground. Once the TUI owns
//! the pane, Herdr pins the state and ignores further reports — a
//! `report-agent ... --state working` for that pane exits 0 and changes
//! nothing, whether it comes from this wrapper or from outside. Letta
//! therefore sits at `idle` even while it is visibly generating.
//!
//! The Infinity bridge does not hit this because Herdr recognises `infinity`
//! and accepts its reports as the agent's own; for an agent Herdr cannot
//! identify, the claim is honoured once and then frozen. Making transitions
//! work needs either Herdr-side support for Letta or an `agent-detection`
//! manifest that actually matches its TUI — neither available here.

use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use herdr_integration::herdr::{self, AgentKind, AgentState, Reporter};
use herdr_integration::letta;

/// How often to re-read the conversation log.
///
/// Letta writes a line per turn boundary, not per token, so a slow poll is
/// enough and keeps the wrapper off the CPU while a model generates.
const POLL: Duration = Duration::from_millis(750);

const USAGE: &str = "\
herdr-letta — run Letta Code in a Herdr pane and report its state

Usage:
  herdr-letta [--] [letta arguments...]
      Run Letta in this pane and report idle/working to Herdr.
      Outside Herdr this execs the real `letta` binary.

  herdr-letta doctor
      Print what the integration can see (Herdr env, letta binary,
      local backend, conversations).

  herdr-letta --help

Environment:
  LETTA_BIN      Path to the real letta binary (default: search PATH)
  HERDR_ENV, HERDR_PANE_ID, HERDR_BIN_PATH
                 Injected by Herdr; reporting is disabled without them.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("herdr-letta: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: Vec<String>) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("doctor") => doctor(),
        _ => run(args),
    }
}

/// Find the real `letta`, skipping this wrapper if it shadows it on PATH.
fn find_letta() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("LETTA_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "LETTA_BIN points at {}, which is not a file",
            path.display()
        ));
    }

    let self_path = std::env::current_exe().ok();
    let paths = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join("letta");
        if !candidate.is_file() {
            continue;
        }
        // Never re-exec ourselves through a shim.
        if let (Some(mine), Ok(theirs)) = (&self_path, candidate.canonicalize()) {
            if mine.canonicalize().map(|m| m == theirs).unwrap_or(false) {
                continue;
            }
        }
        return Ok(candidate);
    }
    Err("could not find `letta` on PATH; install it with \
         `npm install -g @letta-ai/letta-code` or set LETTA_BIN"
        .to_owned())
}

fn run(mut args: Vec<String>) -> Result<ExitCode, String> {
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    let real = find_letta()?;

    // Outside Herdr there is nothing to report to: hand over the terminal so
    // signals and the TTY behave exactly as they would without the wrapper.
    if !herdr::active() {
        use std::os::unix::process::CommandExt;
        let error = Command::new(&real).args(&args).exec();
        return Err(format!("failed to exec {}: {error}", real.display()));
    }

    let pane_cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read current directory: {error}"))?;
    let explicit = session_from_args(&args);

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        // A flag handler is reset to SIG_DFL by exec, so the child keeps
        // normal Ctrl-C behaviour; SIG_IGN would be inherited and break it.
        let _ = signal_hook::flag::register(signal, Arc::clone(&stop));
    }

    let reporter = Reporter::for_agent(AgentKind::LETTA);
    // Claim the pane immediately so Letta appears in the sidebar during the
    // seconds before its first log line exists.
    reporter.report(AgentState::Idle, None, None, false);

    let mut child = Command::new(&real)
        .args(&args)
        .stdin(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", real.display()))?;

    let status = observe(&reporter, &mut child, explicit.as_deref(), &pane_cwd, &stop);

    reporter.release();
    reporter.flush(Duration::from_secs(2));

    match status {
        Some(status) => Ok(ExitCode::from(exit_code(status))),
        None => Ok(ExitCode::FAILURE),
    }
}

/// Poll the local backend until the child exits.
fn observe(
    reporter: &Reporter,
    child: &mut std::process::Child,
    explicit: Option<&str>,
    pane_cwd: &std::path::Path,
    stop: &AtomicBool,
) -> Option<std::process::ExitStatus> {
    let backend = letta::backend_dir();
    let mut last: Option<(AgentState, Option<String>)> = None;
    let mut next_poll = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(error) => {
                herdr::log(&format!("failed to poll letta: {error}"));
                return child.wait().ok();
            }
        }

        if Instant::now() >= next_poll {
            next_poll = Instant::now() + POLL;
            let found = letta::conversations(&backend);
            if let Some(conversation) = letta::select(&found, explicit, pane_cwd) {
                if let Some(state) = letta::observe(conversation) {
                    let current = (state, Some(conversation.agent_id.clone()));
                    if last.as_ref() != Some(&current) {
                        // Report the agent id, not the conversation id: that
                        // is what `letta --agent` resumes.
                        reporter.report(state, None, Some(&conversation.agent_id), true);
                        last = Some(current);
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    child.wait().ok()
}

/// Extract `--agent`/`-a` or `--name`/`-n` from the arguments passed to Letta.
fn session_from_args(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "-a" | "--agent" | "-n" | "--name") {
            return args.get(index + 1).cloned();
        }
        for prefix in ["--agent=", "--name="] {
            if let Some(value) = arg.strip_prefix(prefix) {
                return Some(value.to_owned());
            }
        }
        index += 1;
    }
    None
}

fn exit_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code as u8,
        None => 128u8.saturating_add(status.signal().unwrap_or(0) as u8),
    }
}

fn doctor() -> Result<ExitCode, String> {
    println!("herdr active: {}", herdr::active());
    println!(
        "  HERDR_ENV={:?} HERDR_PANE_ID={:?}",
        std::env::var("HERDR_ENV").ok(),
        std::env::var("HERDR_PANE_ID").ok()
    );
    match find_letta() {
        Ok(path) => println!("letta binary: {}", path.display()),
        Err(error) => println!("letta binary: NOT FOUND ({error})"),
    }

    let backend = letta::backend_dir();
    println!(
        "local backend: {} (exists: {})",
        backend.display(),
        backend.exists()
    );

    let found = letta::conversations(&backend);
    println!("conversations: {}", found.len());
    for conversation in found.iter().take(10) {
        println!(
            "  {}  agent={}  archived={}  cwd={}",
            conversation.id,
            conversation.agent_id,
            conversation.archived,
            conversation
                .cwd
                .as_deref()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_owned())
        );
    }

    let memories = letta::memfs_dirs(&backend);
    println!("memfs directories: {}", memories.len());
    let mut agents: Vec<_> = memories.iter().collect();
    agents.sort();
    for (agent, dir) in agents.into_iter().take(10) {
        println!("  {agent} -> {}", dir.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_agent_selector() {
        let args = ["--agent", "agent-local-1"].map(str::to_owned);
        assert_eq!(session_from_args(&args), Some("agent-local-1".to_owned()));
        let args = ["-a", "x"].map(str::to_owned);
        assert_eq!(session_from_args(&args), Some("x".to_owned()));
        let args = ["--name=Blank"].map(str::to_owned);
        assert_eq!(session_from_args(&args), Some("Blank".to_owned()));
        let args = ["-n", "Blank"].map(str::to_owned);
        assert_eq!(session_from_args(&args), Some("Blank".to_owned()));
        // Unrelated flags must not be mistaken for a selector.
        let args = ["-p", "hello", "--model", "ollama/x"].map(str::to_owned);
        assert_eq!(session_from_args(&args), None);
    }
}
