//! Validated configuration boundary for future outbound MCP STDIO servers.
//! This module neither starts a process nor exposes resolved environment values.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAX_ARGS: usize = 32;
const MAX_ENV: usize = 16;
const MAX_VALUE_LEN: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSecretRef {
    pub vault_account: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, McpSecretRef>,
    pub enabled: bool,
    pub revision: u64,
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .field("revision", &self.revision)
            .field("args", &self.args.len())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

pub struct ResolvedMcpEnvironment {
    pub effective_revision: String,
    pub vars: BTreeMap<String, Zeroizing<String>>,
}

impl fmt::Debug for ResolvedMcpEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedMcpEnvironment")
            .field("effective_revision", &self.effective_revision)
            .field("env", &self.vars.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn env_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn environment_override(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "HOME"
            | "SHELL"
            | "ENV"
            | "IFS"
            | "COMSPEC"
            | "PATHEXT"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "RUSTFLAGS"
    ) || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("PYTHON")
}

fn literal(value: &str) -> bool {
    value.len() <= MAX_VALUE_LEN
        && !value.chars().any(char::is_control)
        && !value.contains('$')
        && !value.contains('`')
        && !value.contains('%')
        && !value.starts_with('~')
}

impl McpServerConfig {
    pub fn vault_account(&self, env_name: &str) -> String {
        format!("mcp_env_{}_{}_{}", self.id.len(), self.id, env_name)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !identifier(&self.id, 64) || self.revision == 0 {
            return Err("MCP_CONFIG_INVALID_ID_OR_REVISION");
        }
        if !Path::new(&self.command).is_absolute() || !literal(&self.command) {
            return Err("MCP_CONFIG_INVALID_COMMAND");
        }
        let executable = Path::new(&self.command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if [
            "sh",
            "bash",
            "zsh",
            "fish",
            "cmd.exe",
            "powershell.exe",
            "pwsh",
            "pwsh.exe",
            "dash",
            "env",
        ]
        .contains(&executable.as_str())
        {
            return Err("MCP_CONFIG_SHELL_FORBIDDEN");
        }
        if self.args.len() > MAX_ARGS || self.args.iter().any(|arg| !literal(arg)) {
            return Err("MCP_CONFIG_INVALID_ARGS");
        }
        if self.env.len() > MAX_ENV {
            return Err("MCP_CONFIG_TOO_MANY_ENV");
        }
        for (name, secret_ref) in &self.env {
            if !env_name(name)
                || environment_override(name)
                || secret_ref.vault_account != self.vault_account(name)
            {
                return Err("MCP_CONFIG_INVALID_SECRET_REF");
            }
        }
        Ok(())
    }

    /// Caller supplies an ephemeral, process-local keyed-hash key. A secret change
    /// changes the effective revision without publishing a reversible fingerprint.
    pub fn resolve_with<F>(
        &self,
        revision_key: &[u8; 32],
        user_id: i64,
        mut get_secret: F,
    ) -> Result<ResolvedMcpEnvironment, &'static str>
    where
        F: FnMut(&str) -> Result<Zeroizing<String>, ()>,
    {
        self.validate()?;
        if !self.enabled {
            return Err("MCP_CONFIG_DISABLED");
        }
        let mut hasher = blake3::Hasher::new_keyed(revision_key);
        hasher.update(&user_id.to_le_bytes());
        let config = serde_json::to_vec(self).map_err(|_| "MCP_CONFIG_SERIALIZE_FAILED")?;
        hasher.update(&(config.len() as u64).to_le_bytes());
        hasher.update(&config);
        let mut vars = BTreeMap::new();
        for (name, secret_ref) in &self.env {
            let secret =
                get_secret(&secret_ref.vault_account).map_err(|_| "MCP_SECRET_UNAVAILABLE")?;
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&(secret.len() as u64).to_le_bytes());
            hasher.update(secret.as_bytes());
            vars.insert(name.clone(), secret);
        }
        Ok(ResolvedMcpEnvironment {
            effective_revision: hasher.finalize().to_hex().to_string(),
            vars,
        })
    }

    /// Resolve only the active user's partition. There is deliberately no
    /// fallback to the machine-global legacy vault for MCP environment secrets.
    pub fn resolve_from_active_user(
        &self,
        conn: &Connection,
        root_key: &[u8; 32],
        revision_key: &[u8; 32],
    ) -> Result<ResolvedMcpEnvironment, &'static str> {
        let user_id = crate::user_partitions::active_user_id(conn)
            .map_err(|_| "MCP_USER_UNAVAILABLE")?
            .ok_or("MCP_USER_UNAVAILABLE")?;
        self.resolve_with(revision_key, user_id, |account| {
            crate::user_partitions::get_user_credential_for(conn, root_key, user_id, account)
                .map_err(|_| ())?
                .ok_or(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> McpServerConfig {
        McpServerConfig {
            id: "example".into(),
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec!["/opt/mcp/server.mjs".into()],
            env: BTreeMap::from([(
                "API_KEY".into(),
                McpSecretRef {
                    vault_account: "mcp_env_7_example_API_KEY".into(),
                },
            )]),
            enabled: true,
            revision: 1,
        }
    }

    #[test]
    fn refs_are_scoped_and_shell_expansion_is_rejected() {
        let valid = fixture();
        assert!(valid.validate().is_ok());
        assert_ne!(
            McpServerConfig {
                id: "foo_bar".into(),
                ..fixture()
            }
            .vault_account("BAZ"),
            McpServerConfig {
                id: "foo".into(),
                ..fixture()
            }
            .vault_account("BAR_BAZ")
        );
        for changed in [
            McpServerConfig {
                command: "node".into(),
                ..fixture()
            },
            McpServerConfig {
                command: "/bin/sh".into(),
                ..fixture()
            },
            McpServerConfig {
                args: vec!["$(whoami)".into()],
                ..fixture()
            },
            McpServerConfig {
                args: vec!["${HOME}".into()],
                ..fixture()
            },
            McpServerConfig {
                revision: 0,
                ..fixture()
            },
            McpServerConfig {
                id: "-invalid".into(),
                ..fixture()
            },
            McpServerConfig {
                env: BTreeMap::from([(
                    "API_KEY".into(),
                    McpSecretRef {
                        vault_account: "server_profile_secret".into(),
                    },
                )]),
                ..fixture()
            },
        ] {
            assert!(changed.validate().is_err());
        }
        for name in [
            "PATH",
            "LD_PRELOAD",
            "NODE_OPTIONS",
            "NODE_PATH",
            "PYTHONPATH",
        ] {
            let mut changed = fixture();
            changed.env = BTreeMap::from([(
                name.into(),
                McpSecretRef {
                    vault_account: format!("mcp_env_7_example_{name}"),
                },
            )]);
            assert!(changed.validate().is_err());
        }
        #[cfg(unix)]
        for command in ["/bin/dash", "/usr/bin/pwsh.exe"] {
            assert_eq!(
                McpServerConfig {
                    command: command.into(),
                    ..fixture()
                }
                .validate(),
                Err("MCP_CONFIG_SHELL_FORBIDDEN")
            );
        }
        #[cfg(windows)]
        assert_eq!(
            McpServerConfig {
                command: r"C:\Program Files\PowerShell\7\pwsh.exe".into(),
                args: vec!["-Command".into(), "Write-Output hello".into()],
                ..fixture()
            }
            .validate(),
            Err("MCP_CONFIG_SHELL_FORBIDDEN")
        );
        assert!(serde_json::from_str::<McpServerConfig>(r#"{"id":"example","command":"/usr/bin/node","args":[],"env":{"API_KEY":"plaintext"},"enabled":true,"revision":1}"#).is_err());
    }

    #[test]
    fn resolution_is_redacted_and_changes_revision_after_secret_mutation() {
        let config = fixture();
        let key = [7; 32];
        let resolve = |value: &str| {
            config
                .resolve_with(&key, 1, |account| {
                    assert_eq!(account, "mcp_env_7_example_API_KEY");
                    Ok(Zeroizing::new(value.into()))
                })
                .unwrap()
        };
        let initial = resolve("private-credential");
        let updated = resolve("replacement-credential");
        assert_ne!(initial.effective_revision, updated.effective_revision);
        assert_eq!(
            initial.effective_revision,
            resolve("private-credential").effective_revision
        );
        for view in [
            format!("{config:?}"),
            format!("{initial:?}"),
            initial.effective_revision.clone(),
        ] {
            assert!(!view.contains("private-credential"));
            assert!(!view.contains("/opt/mcp/server.mjs"));
        }
        assert_eq!(initial.vars["API_KEY"].as_str(), "private-credential");
        assert!(matches!(
            config.resolve_with(&key, 1, |_| Err(())),
            Err("MCP_SECRET_UNAVAILABLE")
        ));
        let other_user = config
            .resolve_with(&key, 2, |_| Ok(Zeroizing::new("private-credential".into())))
            .unwrap();
        assert_ne!(initial.effective_revision, other_user.effective_revision);
    }

    #[test]
    fn disabled_configs_never_request_secrets_and_revision_tracks_config_edits() {
        let key = [9; 32];
        let disabled = McpServerConfig {
            enabled: false,
            ..fixture()
        };
        assert!(matches!(
            disabled.resolve_with(&key, 1, |_| panic!("must not resolve")),
            Err("MCP_CONFIG_DISABLED")
        ));
        let first = fixture()
            .resolve_with(&key, 1, |_| Ok(Zeroizing::new("secret".into())))
            .unwrap();
        let changed = McpServerConfig {
            revision: 2,
            ..fixture()
        }
        .resolve_with(&key, 1, |_| Ok(Zeroizing::new("secret".into())))
        .unwrap();
        assert_ne!(first.effective_revision, changed.effective_revision);
    }
}
