//! Locating the real `infinity` binary and installing the optional shim.

use std::path::{Path, PathBuf};

const SHIM_MARKER: &str = "# herdr-infinity shim";

/// Find the real Infinity binary.
///
/// `INFINITY_BIN` wins. Otherwise we scan `PATH` for `infinity`, skipping our
/// own executable and any shim this tool installed, so a shim earlier on
/// `PATH` cannot recurse.
pub fn find_infinity() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("INFINITY_BIN") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("INFINITY_BIN={} is not a file", path.display()));
    }

    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let path_variable = std::env::var_os("PATH").unwrap_or_default();
    let mut saw_shim = false;

    for directory in std::env::split_paths(&path_variable) {
        let candidate = directory.join("infinity");
        if !candidate.is_file() {
            continue;
        }
        let candidate = candidate.canonicalize().unwrap_or(candidate);
        if current.as_ref() == Some(&candidate) || is_shim(&candidate) {
            saw_shim = true;
            continue;
        }
        return Ok(candidate);
    }

    if saw_shim {
        Err("only the herdr-infinity shim is on PATH; install Infinity \
             (`cargo install infinity-agent-cli`) or set INFINITY_BIN"
            .to_owned())
    } else {
        Err("could not find `infinity` on PATH; install it with \
             `cargo install infinity-agent-cli` or set INFINITY_BIN"
            .to_owned())
    }
}

pub fn default_shim_dir() -> PathBuf {
    crate::home_dir()
        .join(".local")
        .join("share")
        .join("herdr-infinity")
        .join("bin")
}

/// Write `<dir>/infinity`, a tiny POSIX shell shim that always routes through
/// `herdr-infinity run` (which itself falls back to exec'ing the real binary
/// outside Herdr).
pub fn install_shim(dir: Option<PathBuf>, force: bool) -> Result<PathBuf, String> {
    let directory = dir.unwrap_or_else(default_shim_dir);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;

    let path = directory.join("infinity");
    if path.exists() && !is_shim(&path) && !force {
        return Err(format!(
            "{} already exists and is not a herdr-infinity shim; use --force to replace it",
            path.display()
        ));
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot resolve herdr-infinity path: {error}"))?;
    let script = format!(
        "#!/bin/sh\n{SHIM_MARKER} — routes `infinity` through herdr-infinity.\n\
         exec {} run -- \"$@\"\n",
        sh_quote(&executable.display().to_string())
    );

    std::fs::write(&path, script)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    set_executable(&path)?;
    Ok(path)
}

pub fn uninstall_shim(dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let directory = dir.unwrap_or_else(default_shim_dir);
    let path = directory.join("infinity");
    if !path.exists() {
        return Err(format!("no shim at {}", path.display()));
    }
    if !is_shim(&path) {
        return Err(format!(
            "{} is not a herdr-infinity shim; refusing to remove it",
            path.display()
        ));
    }
    std::fs::remove_file(&path)
        .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    Ok(path)
}

/// True when the file starts with our marker. Reads only a small prefix, so a
/// binary named `infinity` is not scanned in full.
pub fn is_shim(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut prefix = [0u8; 256];
    let Ok(read) = file.read(&mut prefix) else {
        return false;
    };
    String::from_utf8_lossy(&prefix[..read]).contains(SHIM_MARKER)
}

fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))
}

/// Quote a string for POSIX `sh` using single quotes.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_uninstall_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = install_shim(Some(directory.path().to_path_buf()), false).unwrap();
        assert!(is_shim(&path));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("run -- \"$@\""));

        let removed = uninstall_shim(Some(directory.path().to_path_buf())).unwrap();
        assert_eq!(removed, path);
        assert!(!path.exists());
    }

    #[test]
    fn refuses_to_overwrite_a_foreign_binary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("infinity");
        std::fs::write(&path, "#!/bin/sh\necho real\n").unwrap();
        assert!(install_shim(Some(directory.path().to_path_buf()), false).is_err());
        assert!(install_shim(Some(directory.path().to_path_buf()), true).is_ok());
        assert!(is_shim(&path));
    }

    #[test]
    fn quoting_handles_apostrophes() {
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
