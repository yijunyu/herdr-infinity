//! End-to-end test: fake Infinity daemon + fake Herdr CLI + fake Infinity
//! binary. Verifies the wrapper's observable contract with Herdr.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn frame(payload: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    wire.extend_from_slice(payload.as_bytes());
    wire
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            panic!("herdr-infinity did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(home.join(".infinity")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&bin).unwrap();

        let log = root.path().join("herdr.log");

        write_executable(&bin.join("infinity-real"), "#!/bin/sh\nsleep 1.5\nexit 0\n");
        write_executable(
            &bin.join("herdr-fake"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HERDR_TEST_LOG\"\n",
        );

        let sessions = format!(
            "{{\"sessions\":{{\"sess-1\":{{\"cwd\":\"{}\",\"shut_down\":false,\"idle\":true,\"archived\":false}}}}}}",
            project.display()
        );
        std::fs::write(home.join(".infinity/sessions.json"), sessions).unwrap();

        Self {
            root,
            home,
            project,
            log,
        }
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.root.path().join("bin").join(name)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-infinity"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("INFINITY_BIN", self.bin("infinity-real"))
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "w1:p1")
            .env("HERDR_BIN_PATH", self.bin("herdr-fake"))
            .env("HERDR_TEST_LOG", &self.log)
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn log_contents(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn reports_working_then_idle_and_releases() {
    let fixture = Fixture::new();
    let socket = fixture.home.join(".infinity/daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();

    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();

        let welcome = r#"{"Welcome":{"sessions":{"sess-1":{"title":"demo","last_updated":"2026-10-28T10:00:00Z","total_tokens_used":0,"status":"Idle"}},"available_models":[],"default_model_name":"m","default_context_window":1,"provider_name":"p","remotes":[]}}"#;
        stream.write_all(&frame(welcome)).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let running = r#"{"SessionsUpdated":{"sessions":{"sess-1":{"title":"demo","last_updated":"2026-10-28T10:00:01Z","total_tokens_used":1,"status":"Running"}}}}"#;
        stream.write_all(&frame(running)).unwrap();
        std::thread::sleep(Duration::from_millis(400));

        let idle = r#"{"SessionsUpdated":{"sessions":{"sess-1":{"title":"demo","last_updated":"2026-10-28T10:00:02Z","total_tokens_used":2,"status":"Idle"}}}}"#;
        stream.write_all(&frame(idle)).unwrap();
        std::thread::sleep(Duration::from_millis(800));
    });

    let mut child = fixture
        .command()
        .args(["run", "--", "infinity", "-s", "sess-1"])
        .spawn()
        .unwrap();
    let status = wait_with_timeout(&mut child, Duration::from_secs(15));
    daemon.join().unwrap();

    assert!(status.success(), "wrapper failed: {status:?}");
    let log = fixture.log_contents();

    assert!(
        log.contains("report-agent w1:p1 --source herdr-infinity --agent infinity")
            || log.contains("report-agent"),
        "no report-agent call:\n{log}"
    );
    assert!(
        log.contains("--state working"),
        "missing working report:\n{log}"
    );
    assert!(log.contains("--state idle"), "missing idle report:\n{log}");
    assert!(
        log.contains("--agent-session-id sess-1"),
        "missing session id:\n{log}"
    );
    assert!(
        log.contains("-- herdr-infinity run -- infinity -s sess-1"),
        "missing resume command:\n{log}"
    );
    assert!(
        log.contains("release-agent"),
        "missing release-agent call:\n{log}"
    );
}

#[test]
fn outside_herdr_it_execs_the_real_binary() {
    let fixture = Fixture::new();
    let real = fixture.bin("infinity-real");
    write_executable(&real, "#!/bin/sh\necho real-infinity\nexit 7\n");

    let child = Command::new(env!("CARGO_BIN_EXE_herdr-infinity"))
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", "/usr/bin:/bin")
        .env("INFINITY_BIN", &real)
        .current_dir(&fixture.project)
        .args(["run", "--", "infinity", "hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "real-infinity"
    );
}
