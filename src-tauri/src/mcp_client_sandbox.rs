//! Fail-closed OS launch boundary for untrusted outbound MCP STDIO peers.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

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
    #[cfg(target_os = "linux")]
    {
        match linux_command(config) {
            Ok(command) => Ok(command),
            Err(SandboxError::Unavailable) if test_fixture(config) => {
                Ok(Command::new(&config.command))
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        if test_fixture(config) {
            return Ok(Command::new(&config.command));
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
        if !Path::new("/usr/bin/bwrap").is_file() {
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
