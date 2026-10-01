//! Fail-closed OS launch boundary for untrusted outbound MCP STDIO peers.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

#[cfg(any(test, target_os = "linux"))]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use tokio::process::Command;

use crate::mcp_client_config::McpServerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxError {
    Unavailable,
    InvalidPath,
}

#[cfg(any(test, target_os = "linux"))]
fn test_fixture(config: &McpServerConfig) -> bool {
    #[cfg(test)]
    {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_fixture.mjs");
        config
            .args
            .first()
            .is_some_and(|arg| arg == &path.to_string_lossy().into_owned())
    }
    #[cfg(not(test))]
    {
        let _ = config;
        false
    }
}

/// Test-only transport fixtures can run without an OS sandbox on CI hosts.
/// Production builds never compile this fallback.
#[cfg(test)]
fn fixture_command(config: &McpServerConfig) -> Result<Command, SandboxError> {
    if !test_fixture(config) {
        return Err(SandboxError::Unavailable);
    }
    let mut command = Command::new(&config.command);
    command.args(&config.args);
    Ok(command)
}

/// Clear inherited secrets. Windows' Node fixture needs its OS directory for
/// libuv initialization; production non-Linux launches still fail closed.
pub(crate) fn clear_peer_environment(command: &mut Command) {
    command.env_clear();
    #[cfg(all(test, target_os = "windows"))]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
}

/// A binary on disk does not prove its required flags/user namespaces work.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fixture_sandbox_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let config = McpServerConfig {
            id: "sandbox_probe".into(),
            command: "/usr/bin/true".into(),
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            revision: 1,
        };
        let Ok(mut command) = linux_command(&config) else {
            return false;
        };
        command
            .as_std_mut()
            .env_clear()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

#[cfg(target_os = "linux")]
fn system_path(path: &Path) -> bool {
    ["/usr", "/lib", "/lib64", "/bin", "/sbin"]
        .iter()
        .any(|base| path.starts_with(base))
}

#[cfg(target_os = "linux")]
fn bind_explicit_file(command: &mut Command, path: &Path) -> Result<(), SandboxError> {
    let canonical = path.canonicalize().map_err(|_| SandboxError::InvalidPath)?;
    if !canonical.is_file() {
        return Err(SandboxError::InvalidPath);
    }
    if !system_path(&canonical) {
        command.arg("--ro-bind").arg(&canonical).arg(&canonical);
    }
    if path != canonical && !system_path(path) {
        command.arg("--ro-bind").arg(&canonical).arg(path);
    }
    Ok(())
}

/// Configured files are explicit read-only grants. Directory mounts are not
/// accepted: they would expose unrelated host files to the untrusted peer.
#[cfg(target_os = "linux")]
fn linux_command(config: &McpServerConfig) -> Result<Command, SandboxError> {
    let bwrap = Path::new("/usr/bin/bwrap");
    if !bwrap.is_file() {
        return Err(SandboxError::Unavailable);
    }
    let executable = PathBuf::from(&config.command)
        .canonicalize()
        .map_err(|_| SandboxError::InvalidPath)?;
    if !executable.is_file() {
        return Err(SandboxError::InvalidPath);
    }
    let mut command = Command::new(bwrap);
    command
        .arg("--unshare-all")
        .arg("--unshare-user")
        .arg("--disable-userns")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--ro-bind")
        .arg("/usr")
        .arg("/usr")
        .arg("--ro-bind-try")
        .arg("/lib")
        .arg("/lib")
        .arg("--ro-bind-try")
        .arg("/lib64")
        .arg("/lib64")
        .arg("--ro-bind-try")
        .arg("/bin")
        .arg("/bin")
        .arg("--ro-bind-try")
        .arg("/sbin")
        .arg("/sbin")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp");
    bind_explicit_file(&mut command, Path::new(&config.command))?;
    for arg in &config.args {
        let path = Path::new(arg);
        if path.is_absolute() && path.exists() {
            bind_explicit_file(&mut command, path)?;
        }
    }
    command.arg("--").arg(executable).args(&config.args);
    Ok(command)
}

pub(crate) fn peer_command(config: &McpServerConfig) -> Result<Command, SandboxError> {
    config.validate().map_err(|_| SandboxError::InvalidPath)?;
    #[cfg(all(test, target_os = "linux"))]
    if test_fixture(config) && !fixture_sandbox_available() {
        return fixture_command(config);
    }
    #[cfg(target_os = "linux")]
    {
        match linux_command(config) {
            Ok(command) => Ok(command),
            Err(SandboxError::Unavailable) if test_fixture(config) => {
                #[cfg(test)]
                {
                    fixture_command(config)
                }
                #[cfg(not(test))]
                {
                    Err(SandboxError::Unavailable)
                }
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(test)]
        if test_fixture(config) {
            return fixture_command(config);
        }
        Err(SandboxError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture(args: Vec<String>) -> McpServerConfig {
        McpServerConfig {
            id: "sandbox".into(),
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args,
            env: BTreeMap::new(),
            enabled: true,
            revision: 1,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sandbox_has_no_ambient_home_or_network() {
        if !Path::new("/usr/bin/bwrap").is_file() {
            return;
        }
        let config = fixture(Vec::new());
        let command = peer_command(&config).unwrap();
        let view = format!("{command:?}");
        assert!(view.contains("--unshare-all"));
        assert!(view.contains("--tmpfs"));
        assert!(!view.contains("--share-net"));
        assert!(!view.contains("--bind /home"));
    }

    #[tokio::test]
    async fn unsandboxed_test_fixture_receives_its_script_and_literal_arguments() {
        let executable = if cfg!(windows) { "node.exe" } else { "node" };
        let node = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
            .map(|dir| dir.join(executable))
            .find(|path| path.is_file())
            .expect("Node runtime");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_fixture.mjs");
        let mut config = fixture(vec![script.to_string_lossy().into_owned(), "exit".into()]);
        config.command = node.to_string_lossy().into_owned();
        let mut command = fixture_command(&config).unwrap();
        clear_peer_environment(&mut command);
        let output = command.output().await.unwrap();
        assert_eq!(
            output.status.code(),
            Some(17),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        config.args[0] = "untrusted-other-script.mjs".into();
        assert_eq!(
            fixture_command(&config).err(),
            Some(SandboxError::Unavailable)
        );
    }

    #[test]
    fn invalid_config_cannot_form_a_command() {
        let config = fixture(Vec::new());
        let invalid = McpServerConfig {
            command: "relative".into(),
            ..config
        };
        assert_eq!(
            peer_command(&invalid).err(),
            Some(SandboxError::InvalidPath)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sandbox_hides_unlisted_host_files() {
        if !fixture_sandbox_available() {
            eprintln!("Sandbox integration unavailable: required bubblewrap flags or user namespaces unsupported");
            return;
        }
        let host_file = tempfile::NamedTempFile::new().unwrap();
        let node = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
            .map(|dir| dir.join("node"))
            .find(|path| path.is_file())
            .expect("Node fixture runtime");
        let mut config = fixture(vec![
            "-e".into(),
            "const fs=require('fs');process.stdout.write(fs.existsSync(process.env.PROBE_PATH)?'visible':'hidden')".into(),
        ]);
        config.command = node.to_string_lossy().into_owned();
        let mut command = peer_command(&config).unwrap();
        command.env_clear().env("PROBE_PATH", host_file.path());
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hidden");
    }
}
