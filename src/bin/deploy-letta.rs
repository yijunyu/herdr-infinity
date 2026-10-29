//! Deploy Letta Code to a remote host over SSH.
//!
//! Installing Letta is a handful of steps that each fail in their own way:
//! Node may be too old or absent, `npm -g` may land outside `PATH`, the
//! backend may default to Cloud, a local inference engine may be on a
//! non-default port, and the chosen model may not support tool calls. Doing
//! that by hand once is fine; doing it for several hosts, and again after a
//! reinstall, is where a typed, re-runnable driver pays for itself.
//!
//! The driver is **idempotent and read-only until it has to write**: every step
//! probes first and skips work that is already done, so re-running it on a
//! configured host reports a clean no-op rather than reinstalling. `--dry-run`
//! prints the plan without touching the remote at all.
//!
//! Usage:
//!   deploy-letta <host>... [--dry-run] [--engine auto|ollama|llama-cpp|none]
//!                          [--llama-cpp-url <url>] [--model <handle>]
//!                          [--verify] [--timeout <seconds>]
//!
//! `<host>` is anything SSH resolves: an `~/.ssh/config` alias or
//! `user@hostname`. Nothing is installed without a reachable host, and a
//! failure on one host does not stop the others.

use std::fmt::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Minimum Node version Letta Code requires.
const NODE_MIN_MAJOR: u32 = 22;
const NODE_MIN_MINOR: u32 = 19;

/// Letta's system prompt and tool definitions need roughly this much context.
/// A model served with less will have its memory and persona truncated, so the
/// driver warns rather than pretending the host is ready.
const LETTA_CONTEXT_FLOOR: u32 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Auto,
    Ollama,
    LlamaCpp,
    None,
}

impl Engine {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "ollama" => Ok(Self::Ollama),
            "llama-cpp" | "llama_cpp" | "llamacpp" => Ok(Self::LlamaCpp),
            "none" => Ok(Self::None),
            other => Err(format!(
                "unknown engine {other:?}; expected auto, ollama, llama-cpp, or none"
            )),
        }
    }
}

struct Options {
    hosts: Vec<String>,
    /// Jump host for targets only reachable from another machine.
    via: Option<String>,
    dry_run: bool,
    engine: Engine,
    llama_cpp_url: Option<String>,
    model: Option<String>,
    verify: bool,
    timeout: Duration,
}

fn main() -> std::process::ExitCode {
    let options = match parse_args(std::env::args().skip(1).collect()) {
        Ok(Some(options)) => options,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("deploy-letta: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut failures = 0;
    for host in &options.hosts {
        println!("\n=== {host} ===");
        let target = Target {
            host: host.clone(),
            via: options.via.clone(),
        };
        match deploy(&target, &options) {
            Ok(report) => print!("{report}"),
            Err(error) => {
                // One unreachable host must not abandon the rest.
                failures += 1;
                eprintln!("  FAILED: {error}");
            }
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} of {} host(s) failed", options.hosts.len());
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

const USAGE: &str = "\
deploy-letta — install and configure Letta Code on remote hosts

Usage:
  deploy-letta <host>... [options]

Options:
  --via <host>           Reach the target through this jump host (ssh -J),
                         for a target only resolvable on another network.
  --dry-run              Print the plan; touch nothing.
  --engine <e>           auto (default), ollama, llama-cpp, none.
  --llama-cpp-url <url>  Base URL for llama.cpp (default: probe :8080, :18081).
  --model <handle>       Model to verify with, e.g. ollama/qwen2.5-coder:7b.
  --verify               Run a real agent turn after installing (slow).
  --timeout <seconds>    Per-SSH-command timeout (default 300).
  -h, --help             Show this help.

A host is any SSH target: an ~/.ssh/config alias or user@hostname.
Each step probes before acting, so re-running is a no-op on a ready host.
";

fn parse_args(args: Vec<String>) -> Result<Option<Options>, String> {
    let mut hosts = Vec::new();
    let mut via = None;
    let mut dry_run = false;
    let mut engine = Engine::Auto;
    let mut llama_cpp_url = None;
    let mut model = None;
    let mut verify = false;
    let mut timeout = Duration::from_secs(300);

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--dry-run" | "-n" => {
                dry_run = true;
                index += 1;
            }
            "--verify" => {
                verify = true;
                index += 1;
            }
            "--via" => {
                via = Some(need(&args, index, "--via")?.to_owned());
                index += 2;
            }
            "--engine" => {
                engine = Engine::parse(need(&args, index, "--engine")?)?;
                index += 2;
            }
            "--llama-cpp-url" => {
                llama_cpp_url = Some(need(&args, index, "--llama-cpp-url")?.to_owned());
                index += 2;
            }
            "--model" => {
                model = Some(need(&args, index, "--model")?.to_owned());
                index += 2;
            }
            "--timeout" => {
                let seconds: u64 = need(&args, index, "--timeout")?
                    .parse()
                    .map_err(|_| "--timeout needs a number of seconds".to_owned())?;
                timeout = Duration::from_secs(seconds);
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected argument {other:?}; try --help"))
            }
            other => {
                hosts.push(other.to_owned());
                index += 1;
            }
        }
    }

    if hosts.is_empty() {
        return Err("no host given; try --help".to_owned());
    }
    Ok(Some(Options {
        hosts,
        via,
        dry_run,
        engine,
        llama_cpp_url,
        model,
        verify,
        timeout,
    }))
}

fn need<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// The result of one remote command.
struct Output {
    status: i32,
    stdout: String,
}

impl Output {
    fn ok(&self) -> bool {
        self.status == 0
    }
    fn trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// Run a command on the remote host over SSH.
///
/// `BatchMode=yes` keeps the driver non-interactive: a host needing a password
/// or an unknown-key confirmation fails fast with a clear message instead of
/// hanging on a prompt nobody can see.
/// A deployment target: the host, plus the jump host needed to reach it.
#[derive(Clone)]
struct Target {
    host: String,
    via: Option<String>,
}

impl Target {
    /// Run a command on this target.
    fn run(&self, script: &str, timeout: Duration) -> Result<Output, String> {
        ssh_via(&self.host, self.via.as_deref(), script, timeout)
    }
}

/// Run a command on `host`, optionally reaching it through a jump host.
///
/// A host on a private network is often resolvable only from a machine on that
/// network, so `--via` lets the driver deploy to it without a VPN or a copied
/// key. SSH performs the hop itself, so nothing is staged on the jump host.
fn ssh_via(
    host: &str,
    via: Option<&str>,
    script: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={}", timeout.as_secs().min(30)));
    if let Some(via) = via {
        command.arg("-J").arg(via);
    }
    command
        .arg(host)
        // A login shell so nvm, Homebrew, and cargo are on PATH exactly as
        // they would be for the user.
        .arg("bash -l -c")
        .arg(shell_quote(script))
        .stdin(Stdio::null());

    let output = command
        .output()
        .map_err(|error| format!("cannot run ssh: {error}"))?;
    let status = output.status.code().unwrap_or(-1);

    // SSH itself failing to connect is distinct from the remote command
    // failing, and only the former is fatal for the host.
    if status == 255 {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "ssh to {host} failed: {}",
            if stderr.is_empty() {
                "connection refused or host unreachable".to_owned()
            } else {
                stderr
            }
        ));
    }

    Ok(Output {
        status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

/// Wrap a script for `bash -l -c`.
fn shell_quote(script: &str) -> String {
    format!("'{}'", script.replace('\'', r"'\''"))
}

/// Parse `v22.19.0` or `22.19.0` into (major, minor).
fn parse_node_version(text: &str) -> Option<(u32, u32)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

fn node_is_new_enough(version: (u32, u32)) -> bool {
    version.0 > NODE_MIN_MAJOR || (version.0 == NODE_MIN_MAJOR && version.1 >= NODE_MIN_MINOR)
}

/// Pick the newest nvm-installed Node that satisfies the minimum.
///
/// A host's default `node` is often an older version (a system package, or an
/// nvm default set years ago) while a new enough one is already installed.
/// Using it avoids installing anything.
fn pick_nvm_node(listing: &str) -> Option<(String, (u32, u32))> {
    let mut best: Option<(String, (u32, u32))> = None;
    for line in listing.split_whitespace() {
        let Some(version) = parse_node_version(line) else {
            continue;
        };
        if !node_is_new_enough(version) {
            continue;
        }
        if best.as_ref().map(|(_, b)| version > *b).unwrap_or(true) {
            best = Some((line.to_owned(), version));
        }
    }
    best
}

/// Which local inference endpoints answered, and what they serve.
#[derive(Debug, Default)]
struct Engines {
    /// Models reported by a *running* Ollama, if it answered.
    ollama: Option<Vec<String>>,
    /// Path to an Ollama binary found while the API was down.
    ollama_binary: Option<String>,
    llama_cpp: Option<String>,
}

fn deploy(target: &Target, options: &Options) -> Result<String, String> {
    let mut report = String::new();
    let timeout = options.timeout;

    // ---- 1. Identify the host -------------------------------------------
    let probe = target.run(
        "uname -s; uname -m; (sw_vers -productVersion 2>/dev/null || true)",
        timeout,
    )?;
    let mut lines = probe.trimmed().lines();
    let os = lines.next().unwrap_or("?").to_owned();
    let arch = lines.next().unwrap_or("?").to_owned();
    let release = lines.next().unwrap_or("").to_owned();
    writeln!(
        report,
        "  host: {os} {arch}{}",
        if release.is_empty() {
            String::new()
        } else {
            format!(" ({release})")
        }
    )
    .ok();

    // ---- 2. Find a usable Node ------------------------------------------
    let node = resolve_node(target, timeout, &mut report)?;

    // ---- 3. Install or confirm Letta Code --------------------------------
    let letta = target.run(
        &format!("{} letta --version 2>/dev/null || true", node.prefix),
        timeout,
    )?;
    let installed = letta.trimmed();
    if installed.is_empty() {
        if options.dry_run {
            writeln!(report, "  letta: WOULD INSTALL (@letta-ai/letta-code)").ok();
        } else {
            writeln!(report, "  letta: installing…").ok();
            let install = target.run(
                &format!(
                    "{} npm install -g @letta-ai/letta-code 2>&1 | tail -3",
                    node.prefix
                ),
                timeout,
            )?;
            if !install.ok() {
                return Err(format!("npm install failed: {}", install.trimmed()));
            }
            let check = target.run(
                &format!("{} letta --version 2>/dev/null || true", node.prefix),
                timeout,
            )?;
            if check.trimmed().is_empty() {
                return Err("letta still not on PATH after install".to_owned());
            }
            writeln!(report, "  letta: installed {}", check.trimmed()).ok();
        }
    } else {
        writeln!(report, "  letta: present ({installed})").ok();
    }

    // ---- 4. Local backend ------------------------------------------------
    // Cloud is the default and needs an account; local keeps all state on the
    // host, which is the whole point of deploying it there.
    let backend = target.run(
        &format!("{} letta backend 2>/dev/null || true", node.prefix),
        timeout,
    )?;
    if backend_is_local(backend.trimmed()) {
        writeln!(report, "  backend: already local").ok();
    } else if options.dry_run {
        writeln!(report, "  backend: WOULD SET to local").ok();
    } else {
        let set = target.run(
            &format!("{} letta backend local 2>&1 | tail -2", node.prefix),
            timeout,
        )?;
        if !set.ok() {
            return Err(format!(
                "could not set the local backend: {}",
                summarize(set.trimmed())
            ));
        }
        // Confirm by re-reading rather than trusting the write: a backend left
        // on Cloud needs an account, which defeats the deployment.
        let confirm = target.run(
            &format!("{} letta backend 2>/dev/null || true", node.prefix),
            timeout,
        )?;
        if backend_is_local(confirm.trimmed()) {
            writeln!(report, "  backend: set local").ok();
        } else {
            return Err(format!(
                "backend did not switch to local; it still reports: {}",
                summarize(confirm.trimmed())
            ));
        }
    }

    // ---- 5. Local inference engines --------------------------------------
    let engines = probe_engines(target, timeout, options)?;
    report.push_str(&render_engines(&engines));

    if options.engine != Engine::None {
        connect_engines(target, &node, &engines, options, timeout, &mut report)?;
    }

    // ---- 6. Model suitability -------------------------------------------
    report.push_str(&check_models(target, &engines, options, timeout)?);

    // ---- 7. Optional end-to-end turn ------------------------------------
    if options.verify && !options.dry_run {
        report.push_str(&verify_turn(target, &node, options, timeout)?);
    }

    Ok(report)
}

/// A Node installation on the remote host, and the shell prefix that exposes it.
struct Node {
    prefix: String,
}

fn resolve_node(target: &Target, timeout: Duration, report: &mut String) -> Result<Node, String> {
    // The default node first: if it is new enough, nothing else matters.
    let default = target.run("node --version 2>/dev/null || true", timeout)?;
    if let Some(version) = parse_node_version(default.trimmed()) {
        if node_is_new_enough(version) {
            writeln!(report, "  node: {}.{} (default)", version.0, version.1).ok();
            return Ok(Node {
                prefix: String::new(),
            });
        }
    }

    // Otherwise look for a new enough nvm install before suggesting one.
    let listing = target.run(
        "ls ~/.nvm/versions/node 2>/dev/null | tr '\\n' ' ' || true",
        timeout,
    )?;
    if let Some((name, version)) = pick_nvm_node(listing.trimmed()) {
        writeln!(
            report,
            "  node: {}.{} (nvm {name}; not the default)",
            version.0, version.1
        )
        .ok();
        return Ok(Node {
            prefix: format!("export PATH=\"$HOME/.nvm/versions/node/{name}/bin:$PATH\";"),
        });
    }

    let found = default.trimmed();
    Err(format!(
        "no Node >= {NODE_MIN_MAJOR}.{NODE_MIN_MINOR} on {} ({}). Install one, \
         e.g. `nvm install {NODE_MIN_MAJOR}` or `brew install node`, then re-run.",
        target.host,
        if found.is_empty() {
            "node not found"
        } else {
            found
        }
    ))
}

fn probe_engines(target: &Target, timeout: Duration, options: &Options) -> Result<Engines, String> {
    let mut engines = Engines::default();

    if matches!(options.engine, Engine::Auto | Engine::Ollama) {
        let tags = target.run(
            "curl -sf --max-time 5 http://127.0.0.1:11434/api/tags 2>/dev/null || true",
            timeout,
        )?;
        if !tags.trimmed().is_empty() {
            engines.ollama = Some(parse_ollama_models(tags.trimmed()));
        } else {
            // A stopped Ollama is a different problem from an absent one, and
            // the fix differs: start the service vs install it. The binary may
            // also sit in a prefix that a non-interactive shell does not
            // search, so look in the usual ones explicitly.
            let binary = target.run(
                "command -v ollama 2>/dev/null \
                 || ls /opt/homebrew/bin/ollama /usr/local/bin/ollama \
                       /snap/bin/ollama 2>/dev/null \
                 | head -1 || true",
                timeout,
            )?;
            engines.ollama_binary = (!binary.trimmed().is_empty())
                .then(|| binary.trimmed().lines().next().unwrap_or("").to_owned());
        }
    }

    if matches!(options.engine, Engine::Auto | Engine::LlamaCpp) {
        // An explicit URL wins; otherwise try the conventional port and the
        // one this project has seen in practice.
        let candidates: Vec<String> = match &options.llama_cpp_url {
            Some(url) => vec![url.clone()],
            None => vec![
                "http://127.0.0.1:8080/v1".to_owned(),
                "http://127.0.0.1:18081/v1".to_owned(),
            ],
        };
        for url in candidates {
            let probe = target.run(
                &format!("curl -sf --max-time 5 {url}/models 2>/dev/null || true"),
                timeout,
            )?;
            if !probe.trimmed().is_empty() {
                engines.llama_cpp = Some(url);
                break;
            }
        }
    }

    Ok(engines)
}

/// Pull model names out of Ollama's `/api/tags` without a JSON dependency.
fn parse_ollama_models(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("\"name\":") {
        rest = &rest[start + 7..];
        let Some(open) = rest.find('"') else { break };
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        names.push(after[..close].to_owned());
        rest = &after[close..];
    }
    names
}

fn render_engines(engines: &Engines) -> String {
    let mut out = String::new();
    match &engines.ollama {
        Some(models) if models.is_empty() => {
            writeln!(out, "  ollama: running, no models pulled").ok();
        }
        Some(models) => {
            writeln!(out, "  ollama: running ({} model(s))", models.len()).ok();
            for name in models.iter().take(8) {
                writeln!(out, "    - {name}").ok();
            }
        }
        None => match &engines.ollama_binary {
            Some(path) => {
                writeln!(
                    out,
                    "  ollama: installed at {path} but NOT running \
                     (start it: `brew services start ollama`, or `ollama serve`)"
                )
                .ok();
            }
            None => {
                writeln!(out, "  ollama: not installed and not reachable on :11434").ok();
            }
        },
    }
    match &engines.llama_cpp {
        Some(url) => {
            writeln!(out, "  llama.cpp: running at {url}").ok();
        }
        None => {
            writeln!(out, "  llama.cpp: not reachable").ok();
        }
    }
    out
}

fn connect_engines(
    target: &Target,
    node: &Node,
    engines: &Engines,
    options: &Options,
    timeout: Duration,
    report: &mut String,
) -> Result<(), String> {
    // `letta connect` is idempotent, but it is still a write, so dry-run must
    // not call it.
    if engines.ollama.is_some() {
        if options.dry_run {
            writeln!(report, "  connect: WOULD connect ollama").ok();
        } else {
            let out = target.run(
                &format!("{} letta connect ollama 2>&1 | tail -2", node.prefix),
                timeout,
            )?;
            writeln!(report, "  connect ollama: {}", summarize(out.trimmed())).ok();
        }
    }

    if let Some(url) = &engines.llama_cpp {
        if options.dry_run {
            writeln!(report, "  connect: WOULD connect llama-cpp at {url}").ok();
        } else {
            // The provider defaults to :8080, so a server anywhere else needs
            // the base URL passed explicitly.
            let out = target.run(
                &format!(
                    "{} LLAMA_CPP_BASE_URL={url} letta connect llama-cpp 2>&1 | tail -2",
                    node.prefix
                ),
                timeout,
            )?;
            writeln!(report, "  connect llama-cpp: {}", summarize(out.trimmed())).ok();
        }
    }

    Ok(())
}

/// True when `letta backend` reports the local backend as the current default.
///
/// The output also names the commands for switching, as in "Run `letta backend
/// cloud` or `letta backend local` to change it", so a bare substring search
/// for "local" matches even when the backend is Cloud. Only the line stating
/// the current default counts.
fn backend_is_local(output: &str) -> bool {
    output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("default backend") || lower.contains("backend set to")
        })
        .any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("local") && !lower.contains("cloud")
        })
}

fn summarize(text: &str) -> String {
    text.lines()
        .last()
        .unwrap_or("(no output)")
        .trim()
        .to_owned()
}

/// Warn about models that cannot drive an agent.
///
/// Two independent failure modes, both of which look like a hung or silent
/// agent rather than an error: a model without tool-call support, and a model
/// served with too small a context for Letta's system prompt.
fn check_models(
    target: &Target,
    engines: &Engines,
    options: &Options,
    timeout: Duration,
) -> Result<String, String> {
    let mut out = String::new();
    let Some(models) = &engines.ollama else {
        return Ok(out);
    };
    if models.is_empty() {
        writeln!(
            out,
            "  models: none pulled; Letta needs a tool-capable model \
             (e.g. `ollama pull qwen2.5-coder:7b`)"
        )
        .ok();
        return Ok(out);
    }

    let mut capable = Vec::new();
    let mut incapable = Vec::new();
    for name in models {
        let show = target.run(
            &format!(
                "curl -sf --max-time 10 http://127.0.0.1:11434/api/show \
                 -d '{{\"name\":\"{name}\"}}' 2>/dev/null || true"
            ),
            timeout,
        )?;
        let body = show.trimmed();
        if body.is_empty() {
            continue;
        }
        // `capabilities` lists "tools" when the model's template supports them.
        if capabilities_include_tools(body) {
            capable.push(name.clone());
        } else {
            incapable.push(name.clone());
        }
    }

    if capable.is_empty() {
        writeln!(
            out,
            "  models: NONE support tool calls — Letta will return empty turns"
        )
        .ok();
        if !incapable.is_empty() {
            writeln!(out, "    without tools: {}", incapable.join(", ")).ok();
        }
        writeln!(
            out,
            "    pull one that does: qwen2.5-coder, qwen3, llama3.1 or newer"
        )
        .ok();
    } else {
        writeln!(out, "  models with tool support: {}", capable.join(", ")).ok();
        writeln!(
            out,
            "    note: Letta needs ~{}k context; raise it with a derived model \
             (PARAMETER num_ctx) or OLLAMA_CONTEXT_LENGTH",
            LETTA_CONTEXT_FLOOR / 1000
        )
        .ok();
    }

    if let Some(model) = &options.model {
        writeln!(out, "  requested model: {model}").ok();
    }
    Ok(out)
}

/// True when an `/api/show` body advertises tool support.
fn capabilities_include_tools(body: &str) -> bool {
    let Some(start) = body.find("\"capabilities\"") else {
        return false;
    };
    let rest = &body[start..];
    let end = rest.find(']').unwrap_or(rest.len());
    rest[..end].contains("\"tools\"")
}

/// Run one real agent turn, the only check that proves the host can serve.
fn verify_turn(
    target: &Target,
    node: &Node,
    options: &Options,
    timeout: Duration,
) -> Result<String, String> {
    let mut out = String::new();
    let Some(model) = &options.model else {
        writeln!(
            out,
            "  verify: skipped (pass --model to choose which model to test)"
        )
        .ok();
        return Ok(out);
    };

    writeln!(out, "  verify: running one turn on {model}…").ok();
    let turn = target.run(
        &format!(
            "{} letta -p 'Reply with exactly: LOCAL OK' --model {model} \
             --output-format text 2>&1 | tail -6",
            node.prefix
        ),
        timeout,
    )?;
    let body = turn.trimmed();
    if body.contains("LOCAL OK") {
        writeln!(out, "  verify: PASS — the host completed an agent turn").ok();
    } else if body.is_empty() {
        writeln!(
            out,
            "  verify: FAIL — empty turn. The model produced no reply; check \
             tool support and the served context window."
        )
        .ok();
    } else {
        writeln!(out, "  verify: FAIL — {}", summarize(body)).ok();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_versions() {
        assert_eq!(parse_node_version("v22.19.0"), Some((22, 19)));
        assert_eq!(parse_node_version("22.23.2"), Some((22, 23)));
        assert_eq!(parse_node_version("v18.20.8"), Some((18, 20)));
        assert_eq!(parse_node_version(""), None);
        assert_eq!(parse_node_version("not a version"), None);
    }

    #[test]
    fn enforces_the_letta_node_minimum() {
        assert!(node_is_new_enough((22, 19)));
        assert!(node_is_new_enough((22, 23)));
        assert!(node_is_new_enough((25, 0)));
        // Letta requires 22.19; 22.18 and the whole 18.x line are too old.
        assert!(!node_is_new_enough((22, 18)));
        assert!(!node_is_new_enough((18, 20)));
    }

    #[test]
    fn picks_the_newest_suitable_nvm_node() {
        let listing = "v18.20.8 v20.0.0 v22.19.0 v25.5.0";
        let (name, version) = pick_nvm_node(listing).unwrap();
        assert_eq!(name, "v25.5.0");
        assert_eq!(version, (25, 5));
    }

    #[test]
    fn ignores_nvm_nodes_that_are_too_old() {
        assert!(pick_nvm_node("v18.20.8 v20.0.0").is_none());
        assert!(pick_nvm_node("").is_none());
        // Exactly the minimum is acceptable.
        assert_eq!(pick_nvm_node("v22.19.0").unwrap().0, "v22.19.0");
    }

    #[test]
    fn extracts_ollama_model_names() {
        let body = r#"{"models":[{"name":"qwen2.5-coder:7b","size":4700000000},
                       {"name":"llama3:latest","size":4700000000}]}"#;
        assert_eq!(
            parse_ollama_models(body),
            ["qwen2.5-coder:7b", "llama3:latest"]
        );
        assert!(parse_ollama_models(r#"{"models":[]}"#).is_empty());
        assert!(parse_ollama_models("not json").is_empty());
    }

    #[test]
    fn detects_tool_capability() {
        assert!(capabilities_include_tools(
            r#"{"capabilities":["completion","tools","insert"]}"#
        ));
        assert!(!capabilities_include_tools(
            r#"{"capabilities":["completion","insert"]}"#
        ));
        // "tools" appearing elsewhere must not count.
        assert!(!capabilities_include_tools(
            r#"{"template":"{{ .Tools }} tools","capabilities":["completion"]}"#
        ));
        assert!(!capabilities_include_tools("{}"));
    }

    #[test]
    fn quotes_scripts_safely_for_a_login_shell() {
        assert_eq!(shell_quote("echo hi"), "'echo hi'");
        // A single quote in the script must not terminate the wrapper.
        assert_eq!(shell_quote("echo 'x'"), r"'echo '\''x'\'''");
    }

    #[test]
    fn engine_names_parse_with_aliases() {
        assert_eq!(Engine::parse("auto").unwrap(), Engine::Auto);
        assert_eq!(Engine::parse("ollama").unwrap(), Engine::Ollama);
        assert_eq!(Engine::parse("llama-cpp").unwrap(), Engine::LlamaCpp);
        assert_eq!(Engine::parse("llamacpp").unwrap(), Engine::LlamaCpp);
        assert_eq!(Engine::parse("none").unwrap(), Engine::None);
        assert!(Engine::parse("gpu").is_err());
    }

    #[test]
    fn requires_at_least_one_host() {
        assert!(parse_args(vec![]).is_err());
        let options = parse_args(vec!["studio".to_owned(), "--dry-run".to_owned()])
            .unwrap()
            .unwrap();
        assert_eq!(options.hosts, ["studio"]);
        assert!(options.dry_run);
    }

    #[test]
    fn parses_several_hosts_and_options() {
        let options = parse_args(
            [
                "studio",
                "amd",
                "--engine",
                "ollama",
                "--model",
                "ollama/qwen3",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.hosts, ["studio", "amd"]);
        assert_eq!(options.engine, Engine::Ollama);
        assert_eq!(options.model.as_deref(), Some("ollama/qwen3"));
    }

    #[test]
    fn rejects_flags_missing_their_value() {
        assert!(parse_args(vec!["h".to_owned(), "--engine".to_owned()]).is_err());
        assert!(parse_args(vec![
            "h".to_owned(),
            "--timeout".to_owned(),
            "soon".to_owned()
        ])
        .is_err());
    }

    #[test]
    fn backend_detection_is_not_fooled_by_the_hint_line() {
        // Verbatim Cloud output: the hint line names `letta backend local`, so
        // a substring search for "local" wrongly reports success.
        let cloud = "Default backend: Letta Cloud (cloud)\n\
                     Run `letta backend cloud` or `letta backend local` to change it.";
        assert!(!backend_is_local(cloud));

        let local = "Default backend: local mode (local)\n\
                     Run `letta backend cloud` or `letta backend local` to change it.";
        assert!(backend_is_local(local));

        // The confirmation printed right after switching.
        assert!(backend_is_local("Default backend set to local mode."));
        assert!(!backend_is_local(""));
    }

    #[test]
    fn accepts_a_jump_host() {
        let options = parse_args(
            ["amd", "--via", "studio"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.hosts, ["amd"]);
        assert_eq!(options.via.as_deref(), Some("studio"));

        // Absent by default, and the flag needs a value.
        let plain = parse_args(vec!["amd".to_owned()]).unwrap().unwrap();
        assert_eq!(plain.via, None);
        assert!(parse_args(vec!["amd".to_owned(), "--via".to_owned()]).is_err());
    }

    #[test]
    fn renders_engine_findings() {
        let engines = Engines {
            ollama: Some(vec!["qwen2.5-coder:7b".to_owned()]),
            ollama_binary: None,
            llama_cpp: Some("http://127.0.0.1:18081/v1".to_owned()),
        };
        let text = render_engines(&engines);
        assert!(text.contains("ollama: running (1 model(s))"));
        assert!(text.contains("qwen2.5-coder:7b"));
        assert!(text.contains("18081"));

        let empty = render_engines(&Engines::default());
        assert!(empty.contains("not installed and not reachable"));
        assert!(empty.contains("llama.cpp: not reachable"));

        // Installed but stopped is a distinct, actionable state: studio had
        // Ollama in /opt/homebrew/bin while the API was down.
        let stopped = render_engines(&Engines {
            ollama: None,
            ollama_binary: Some("/opt/homebrew/bin/ollama".to_owned()),
            llama_cpp: None,
        });
        assert!(stopped.contains("NOT running"), "{stopped}");
        assert!(stopped.contains("brew services start"), "{stopped}");
    }
}
