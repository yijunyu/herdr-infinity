//! Tests for `plugin/run.sh`, the launcher the plugin manifest invokes.
//!
//! Herdr runs plugin commands with the *server's* environment, which in
//! practice is often the bare system PATH: neither `~/.cargo/bin` nor
//! `~/.local/bin` is visible, so a manifest that spells the command
//! `herdr-infinity` fails with ENOENT. The launcher resolves the binary
//! explicitly, and these tests pin that behaviour down.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn launcher() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/run.sh")
}

fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// Run the launcher with an empty PATH, the situation that broke the first
/// version of the manifest.
fn run_with_empty_path(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(launcher())
        .args(args)
        .env("HERDR_PLUGIN_ROOT", root)
        .env("PATH", "")
        .env_remove("HERDR_INFINITY_BIN")
        .output()
        .unwrap()
}

#[test]
fn prefers_an_explicit_override() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("custom-binary");
    write_executable(&binary, "#!/bin/sh\necho override \"$@\"\n");

    let output = Command::new(launcher())
        .arg("doctor")
        .env("HERDR_PLUGIN_ROOT", dir.path())
        .env("HERDR_INFINITY_BIN", &binary)
        .env("PATH", "")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "override doctor"
    );
}

#[test]
fn finds_the_release_build_in_a_linked_checkout() {
    let dir = tempfile::tempdir().unwrap();
    write_executable(
        &dir.path().join("target/release/herdr-infinity"),
        "#!/bin/sh\necho release \"$@\"\n",
    );

    let output = run_with_empty_path(dir.path(), &["sessions", "--all"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "release sessions --all"
    );
}

#[test]
fn release_wins_over_debug() {
    let dir = tempfile::tempdir().unwrap();
    write_executable(
        &dir.path().join("target/debug/herdr-infinity"),
        "#!/bin/sh\necho debug\n",
    );
    write_executable(
        &dir.path().join("target/release/herdr-infinity"),
        "#!/bin/sh\necho release\n",
    );

    let output = run_with_empty_path(dir.path(), &["doctor"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "release");
}

#[test]
fn falls_back_to_a_cargo_install_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let cargo_home = dir.path().join("cargo");
    write_executable(
        &cargo_home.join("bin/herdr-infinity"),
        "#!/bin/sh\necho cargo-install\n",
    );

    // An empty plugin root, so only CARGO_HOME can satisfy the lookup.
    let root = dir.path().join("empty-root");
    std::fs::create_dir_all(&root).unwrap();

    let output = Command::new(launcher())
        .arg("doctor")
        .env("HERDR_PLUGIN_ROOT", &root)
        .env("CARGO_HOME", &cargo_home)
        .env("PATH", "")
        .env_remove("HERDR_INFINITY_BIN")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "cargo-install"
    );
}

#[test]
fn reports_a_useful_error_when_nothing_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    // HOME and CARGO_HOME point at empty directories so no real installation
    // on the test machine can satisfy the lookup.
    let output = Command::new(launcher())
        .arg("doctor")
        .env("HERDR_PLUGIN_ROOT", dir.path())
        .env("HOME", dir.path())
        .env("CARGO_HOME", dir.path().join("no-cargo"))
        .env("PATH", "")
        .env_remove("HERDR_INFINITY_BIN")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("binary not found"), "{stderr}");
    // The message has to tell the user how to fix it.
    assert!(stderr.contains("cargo install"), "{stderr}");
    assert!(stderr.contains("HERDR_INFINITY_BIN"), "{stderr}");
}

#[test]
fn forwards_the_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    write_executable(
        &dir.path().join("target/release/herdr-infinity"),
        "#!/bin/sh\nexit 3\n",
    );

    let output = run_with_empty_path(dir.path(), &["sessions"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn manifest_commands_point_at_the_launcher() {
    // The manifest must not spell a bare `herdr-infinity`: that is exactly the
    // command the Herdr server cannot resolve.
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin.toml"))
            .unwrap();

    for line in manifest.lines() {
        let line = line.trim();
        if let Some(command) = line.strip_prefix("command = ") {
            assert!(
                command.contains("plugin/run.sh"),
                "manifest command does not go through the launcher: {line}"
            );
        }
    }
    assert!(manifest.contains("plugin/run.sh"));
}
