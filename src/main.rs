//! `herdr-infinity` — native Herdr integration for Infinity Code.
//!
//! Infinity Code already separates the agent from its terminal UI: every turn
//! lives in the long-running Infinity daemon, and the TUI is just a client.
//! That makes a faithful Herdr integration possible without patching Infinity:
//! we run the TUI unchanged and observe the daemon's `SessionsUpdated`
//! broadcasts on the side, translating `Running`/`Idle`/`WaitingForChoice`
//! into Herdr's `working`/`idle`/`blocked` states.
//!
//! The wrapper is deliberately a shim around the real `infinity` binary. It
//! never edits Infinity's files and never changes the agent's behaviour.

use herdr_integration::{herdr, infinity, memfs, plugin, shim};

use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use herdr::Reporter;
use infinity::BridgeOptions;

const USAGE: &str = "\
herdr-infinity — native Herdr integration for Infinity Code

Usage:
  herdr-infinity run [--] [infinity arguments...]
      Run Infinity in this pane and report idle/working/blocked to Herdr.
      Outside Herdr this simply execs the real `infinity` binary.

  herdr-infinity sessions [--all] [--here]
      List Infinity sessions. Live sessions only unless --all; --here
      restricts the list to the current directory.

  herdr-infinity open <session-prefix> [--focus]
      Open an existing session in a new Herdr pane.

  herdr-infinity new [--focus]
      Start a fresh Infinity session in a new Herdr pane.

  herdr-infinity memfs <import|export|status> [options]
      Sync memory between Letta's MemFS and Claude Code's memory
      directory. Both store one fact per Markdown file with a
      `description`, so the two interoperate.
        --memory-dir <dir>  MemFS root (default: $MEMORY_DIR)
        --claude-dir <dir>  Claude Code memory dir (default: derived
                            from the current project path)
        --dry-run           Show the plan without writing
        --only-types <list> Copy only these metadata.type values, e.g.
                            feedback,reference. MemFS renders every file
                            into the agent's context each turn, so a large
                            project must be seeded selectively.

  herdr-infinity doctor
      Print what the integration can see (Herdr env, Infinity binary,
      daemon socket, known sessions).

  herdr-infinity install-shim [--dir <dir>] [--force]
      Install an `infinity` shim that routes through herdr-infinity.
      Default directory: ~/.local/share/herdr-infinity/bin

  herdr-infinity uninstall-shim [--dir <dir>]
      Remove a shim installed by this tool.

  herdr-infinity version
  herdr-infinity --help

Environment:
  INFINITY_BIN   Path to the real Infinity binary (default: search PATH)
  HERDR_ENV, HERDR_PANE_ID, HERDR_BIN_PATH
                 Injected by Herdr; reporting is disabled without them.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("herdr-infinity: {err}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: Vec<String>) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") | Some("help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("herdr-infinity {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Some("run") => cmd_run(args[1..].to_vec()),
        Some("sessions") => cmd_sessions(&args[1..]),
        Some("open") => cmd_open(&args[1..]),
        Some("new") => cmd_new(&args[1..]),
        Some("memfs") => cmd_memfs(&args[1..]),
        Some("doctor") => cmd_doctor(),
        Some("install-shim") => cmd_install_shim(&args[1..]),
        Some("uninstall-shim") => cmd_uninstall_shim(&args[1..]),
        Some(other) => Err(format!("unknown command {other:?}; try --help")),
    }
}

fn cmd_run(mut args: Vec<String>) -> Result<ExitCode, String> {
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }

    let real = shim::find_infinity()?;

    // Outside Herdr there is nothing to report to: hand the terminal over to
    // the real binary with exec so signals and the TTY behave exactly as
    // before.
    if !herdr::active() {
        use std::os::unix::process::CommandExt;
        let error = Command::new(&real).args(&args).exec();
        return Err(format!("failed to exec {}: {error}", real.display()));
    }

    let pane_cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read current directory: {error}"))?;
    let explicit = infinity::session_prefix_from_args(&args);
    let follow_after_exit = args.iter().any(|arg| arg == "-H" || arg == "--headless");
    let local = args.iter().any(|arg| arg == "--local" || arg == "-l");

    if local {
        eprintln!(
            "herdr-infinity: --local keeps the agent in this process; Herdr state \
             reporting is unavailable in that mode"
        );
    }

    let baseline = infinity::read_store_sessions();

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        // SIG_IGN would be inherited across exec and break Ctrl-C for the TUI;
        // a flag handler is reset to SIG_DFL by exec, so the child is unaffected.
        let _ = signal_hook::flag::register(signal, Arc::clone(&stop));
    }

    let reporter = Reporter::new();
    let mut child = Command::new(&real)
        .args(&args)
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", real.display()))?;

    let options = BridgeOptions {
        explicit,
        pane_cwd,
        baseline,
        follow_after_exit,
        local,
    };
    let status = infinity::bridge(options, &reporter, &mut child, &stop);

    reporter.release();
    reporter.flush(Duration::from_secs(2));

    match status {
        Some(status) => Ok(ExitCode::from(exit_code(status))),
        None => Ok(ExitCode::FAILURE),
    }
}

fn exit_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code as u8,
        None => 128u8.saturating_add(status.signal().unwrap_or(0) as u8),
    }
}

fn cmd_sessions(args: &[String]) -> Result<ExitCode, String> {
    let mut all = false;
    let mut here = false;
    for arg in args {
        match arg.as_str() {
            "--all" | "-a" => all = true,
            "--here" => here = true,
            other => return Err(format!("unexpected argument {other:?} for sessions")),
        }
    }

    let dir = if here {
        Some(
            std::env::current_dir()
                .map_err(|error| format!("cannot read current directory: {error}"))?,
        )
    } else {
        None
    };

    print!("{}", plugin::list_sessions(all, dir.as_ref()));
    Ok(ExitCode::SUCCESS)
}

fn cmd_open(args: &[String]) -> Result<ExitCode, String> {
    let mut prefix: Option<&str> = None;
    let mut focus = false;
    for arg in args {
        match arg.as_str() {
            "--focus" => focus = true,
            other if other.starts_with('-') => {
                return Err(format!("unexpected argument {other:?} for open"))
            }
            other => {
                if prefix.replace(other).is_some() {
                    return Err("open takes a single session prefix".to_owned());
                }
            }
        }
    }

    let prefix = prefix.ok_or_else(|| "open needs a session id or prefix".to_owned())?;
    require_herdr()?;
    let ctx = plugin::Context::from_env();
    println!("{}", plugin::open_session(&ctx, prefix, focus)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_new(args: &[String]) -> Result<ExitCode, String> {
    let mut focus = false;
    for arg in args {
        match arg.as_str() {
            "--focus" => focus = true,
            other => return Err(format!("unexpected argument {other:?} for new")),
        }
    }

    require_herdr()?;
    let ctx = plugin::Context::from_env();
    println!("{}", plugin::new_session(&ctx, focus)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_memfs(args: &[String]) -> Result<ExitCode, String> {
    let mut memory_dir: Option<PathBuf> = None;
    let mut claude_dir: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut types: Vec<String> = Vec::new();
    let mut mode: Option<&str> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--memory-dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--memory-dir needs a path".to_owned())?;
                memory_dir = Some(PathBuf::from(value));
                index += 2;
            }
            "--claude-dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--claude-dir needs a path".to_owned())?;
                claude_dir = Some(PathBuf::from(value));
                index += 2;
            }
            "--dry-run" | "-n" => {
                dry_run = true;
                index += 1;
            }
            "--only-types" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--only-types needs a comma-separated list".to_owned())?;
                types = value
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect();
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected argument {other:?} for memfs"))
            }
            other => {
                if mode.replace(other).is_some() {
                    return Err("memfs takes a single subcommand".to_owned());
                }
                index += 1;
            }
        }
    }

    let mode = mode.ok_or_else(|| "memfs needs import, export, or status".to_owned())?;
    let memfs_dir = memfs::resolve_memfs_dir(memory_dir)?;
    let project = std::env::current_dir()
        .map_err(|error| format!("cannot read current directory: {error}"))?;
    let claude_dir = memfs::resolve_claude_dir(claude_dir, &project)?;

    match mode {
        "status" => {
            // Report both directions so the user sees the whole picture
            // before choosing one.
            println!(
                "MemFS:       {}",
                memfs::memfs_system_dir(&memfs_dir).display()
            );
            println!("Claude Code: {}\n", claude_dir.display());
            let import = memfs::sync(&memfs_dir, &claude_dir, memfs::Direction::Import, true)?;
            print!("{}", import.render(memfs::Direction::Import));
            let export = memfs::sync(&memfs_dir, &claude_dir, memfs::Direction::Export, true)?;
            print!("{}", export.render(memfs::Direction::Export));
        }
        "import" | "export" => {
            let direction = if mode == "import" {
                memfs::Direction::Import
            } else {
                memfs::Direction::Export
            };
            let filter = memfs::Filter { types };
            let plan = memfs::sync_filtered(&memfs_dir, &claude_dir, direction, dry_run, &filter)?;
            print!("{}", plan.render(direction));
            if dry_run && !plan.is_empty() {
                println!("(dry run: nothing was written)");
            }
        }
        other => return Err(format!("unknown memfs subcommand {other:?}")),
    }
    Ok(ExitCode::SUCCESS)
}

/// Commands that create panes need a live Herdr session to talk to.
fn require_herdr() -> Result<(), String> {
    if herdr::active() {
        return Ok(());
    }
    Err(
        "not running inside a Herdr pane (HERDR_ENV, HERDR_PANE_ID, and \
         HERDR_BIN_PATH must be set)"
            .to_owned(),
    )
}

fn cmd_doctor() -> Result<ExitCode, String> {
    println!("herdr active: {}", herdr::active());
    println!(
        "  HERDR_ENV={:?} HERDR_PANE_ID={:?}",
        std::env::var("HERDR_ENV").ok(),
        std::env::var("HERDR_PANE_ID").ok()
    );
    println!(
        "  HERDR_BIN_PATH={:?}",
        std::env::var("HERDR_BIN_PATH").ok()
    );

    match shim::find_infinity() {
        Ok(path) => println!("infinity binary: {}", path.display()),
        Err(error) => println!("infinity binary: NOT FOUND ({error})"),
    }

    let socket = herdr_integration::home_dir()
        .join(".infinity")
        .join("daemon.sock");
    println!(
        "infinity daemon socket: {} (exists: {})",
        socket.display(),
        socket.exists()
    );

    let store = infinity::read_store_sessions();
    println!("known sessions: {}", store.len());
    let mut entries: Vec<_> = store.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (id, entry) in entries.into_iter().take(20) {
        println!(
            "  {id}  cwd={}  idle={}  shut_down={}  archived={}",
            entry.cwd.display(),
            entry.idle,
            entry.shut_down,
            entry.archived
        );
    }

    println!(
        "current directory: {}",
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("<error: {error}>"))
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_install_shim(args: &[String]) -> Result<ExitCode, String> {
    let mut dir: Option<PathBuf> = None;
    let mut force = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--dir needs a path".to_owned())?;
                dir = Some(PathBuf::from(value));
                index += 2;
            }
            "--force" | "-f" => {
                force = true;
                index += 1;
            }
            other => return Err(format!("unexpected argument {other:?} for install-shim")),
        }
    }

    let path = shim::install_shim(dir, force)?;
    println!("installed shim: {}", path.display());
    println!(
        "put {} before the real infinity binary on PATH, e.g.:",
        path.parent().unwrap_or(&path).display()
    );
    println!(
        "  export PATH=\"{}:$PATH\"",
        path.parent().unwrap_or(&path).display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_uninstall_shim(args: &[String]) -> Result<ExitCode, String> {
    let mut dir: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dir" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--dir needs a path".to_owned())?;
                dir = Some(PathBuf::from(value));
                index += 2;
            }
            other => return Err(format!("unexpected argument {other:?} for uninstall-shim")),
        }
    }

    let path = shim::uninstall_shim(dir)?;
    println!("removed shim: {}", path.display());
    Ok(ExitCode::SUCCESS)
}
