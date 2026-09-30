//! Private, active-user configuration contract for outbound MCP HTTPS servers.
//! No network request, Tauri command, or model tool is exposed here.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::fmt;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use url::{Host, Url};
use zeroize::Zeroizing;

const MAX_ENDPOINT: usize = 2048;
const MAX_CLIENT_METADATA_URL: usize = 2048;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpHttpAuth {
    None,
    Bearer {
        vault_account: String,
    },
    OAuth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id_metadata_url: Option<String>,
    },
}

impl fmt::Debug for McpHttpAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Bearer { .. } => f.write_str("Bearer { vault_account: [redacted] }"),
            Self::OAuth { .. } => f.write_str("OAuth { client_id_metadata_url: [redacted] }"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpHttpServerConfig {
    pub id: String,
    pub endpoint: String,
    pub auth: McpHttpAuth,
    pub enabled: bool,
    pub revision: u64,
}

impl fmt::Debug for McpHttpServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpHttpServerConfig")
            .field("id", &self.id)
            .field("auth", &self.auth)
            .field("enabled", &self.enabled)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

pub(crate) struct ResolvedMcpHttpAuth {
    pub effective_revision: String,
    pub bearer: Option<Zeroizing<String>>,
}

impl fmt::Debug for ResolvedMcpHttpAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedMcpHttpAuth")
            .field("effective_revision", &self.effective_revision)
            .field("bearer_present", &self.bearer.is_some())
            .finish()
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// DNS resolution, proxy selection, and every redirect must be checked again
/// against the actual connected address by the HTTP transport.
fn parse_public_https(value: &str, max_len: usize) -> Result<Url, &'static str> {
    if value.is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
        return Err("MCP_HTTP_INVALID_ENDPOINT");
    }
    let url = Url::parse(value).map_err(|_| "MCP_HTTP_INVALID_ENDPOINT")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err("MCP_HTTP_INVALID_ENDPOINT");
    }
    // IP literals, including URL parser canonicalizations such as 127.1 and
    // integer IPv4 forms, are excluded. DNS answers are checked at connection.
    let Host::Domain(domain) = url.host().ok_or("MCP_HTTP_INVALID_ENDPOINT")? else {
        return Err("MCP_HTTP_INVALID_ENDPOINT");
    };
    let domain = domain.trim_end_matches('.');
    if !domain.contains('.')
        || domain == "localhost"
        || domain.ends_with(".localhost")
        || domain.ends_with(".local")
        || domain.ends_with(".internal")
    {
        return Err("MCP_HTTP_INVALID_ENDPOINT");
    }
    Ok(url)
}

impl McpHttpServerConfig {
    pub fn bearer_vault_account(&self) -> String {
        format!("mcp_http_bearer_{}_{}", self.id.len(), self.id)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_id(&self.id) || self.revision == 0 {
            return Err("MCP_HTTP_INVALID_ID_OR_REVISION");
        }
        parse_public_https(&self.endpoint, MAX_ENDPOINT)?;
        match &self.auth {
            McpHttpAuth::None => {}
            McpHttpAuth::Bearer { vault_account } => {
                if vault_account != &self.bearer_vault_account() {
                    return Err("MCP_HTTP_INVALID_BEARER_REF");
                }
            }
            McpHttpAuth::OAuth {
                client_id_metadata_url,
            } => {
                if let Some(url) = client_id_metadata_url {
                    parse_public_https(url, MAX_CLIENT_METADATA_URL)?;
                }
            }
        }
        Ok(())
    }

    /// A process-local keyed revision invalidates approvals on config, user,
    /// or bearer changes without publishing a reversible secret fingerprint.
    pub fn resolve_with<F>(
        &self,
        revision_key: &[u8; 32],
        user_id: i64,
        mut get_secret: F,
    ) -> Result<ResolvedMcpHttpAuth, &'static str>
    where
        F: FnMut(&str) -> Result<Zeroizing<String>, ()>,
    {
        self.validate()?;
        if !self.enabled {
            return Err("MCP_HTTP_DISABLED");
        }
        let mut hasher = blake3::Hasher::new_keyed(revision_key);
        hasher.update(b"mcp_http_config_v1");
        hasher.update(&user_id.to_le_bytes());
        let config = serde_json::to_vec(self).map_err(|_| "MCP_HTTP_SERIALIZE_FAILED")?;
        hasher.update(&(config.len() as u64).to_le_bytes());
        hasher.update(&config);
        let bearer = match &self.auth {
            McpHttpAuth::Bearer { vault_account } => {
                let secret =
                    get_secret(vault_account).map_err(|_| "MCP_HTTP_SECRET_UNAVAILABLE")?;
                if secret.is_empty() || secret.len() > 4096 || secret.chars().any(char::is_control)
                {
                    return Err("MCP_HTTP_SECRET_INVALID");
                }
                hasher.update(&(secret.len() as u64).to_le_bytes());
                hasher.update(secret.as_bytes());
                Some(secret)
            }
            _ => None,
        };
        Ok(ResolvedMcpHttpAuth {
            effective_revision: hasher.finalize().to_hex().to_string(),
            bearer,
        })
    }

    /// Bearer credentials are read only from the active user's encrypted partition.
    pub fn resolve_from_active_user(
        &self,
        conn: &Connection,
        root_key: &[u8; 32],
        revision_key: &[u8; 32],
    ) -> Result<ResolvedMcpHttpAuth, &'static str> {
        let user_id = crate::user_partitions::active_user_id(conn)
            .map_err(|_| "MCP_HTTP_USER_UNAVAILABLE")?
            .ok_or("MCP_HTTP_USER_UNAVAILABLE")?;
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

    fn fixture() -> McpHttpServerConfig {
        McpHttpServerConfig {
            id: "example".into(),
            endpoint: "https://mcp.example.com/mcp".into(),
            auth: McpHttpAuth::None,
            enabled: true,
            revision: 1,
        }
    }

    #[test]
    fn https_endpoint_excludes_inline_credentials_and_local_targets() {
        assert!(fixture().validate().is_ok());
        for endpoint in [
            "http://mcp.example.com/mcp",
            "https://user:pass@mcp.example.com/mcp",
            "https://mcp.example.com/mcp#frag",
            "https://mcp.example.com/mcp?token=secret",
            "https://127.0.0.1/mcp",
            "https://127.1/mcp",
            "https://10.0.0.2/mcp",
            "https://169.254.1.1/mcp",
            "https://[::1]/mcp",
            "https://localhost/mcp",
            "https://peer.local/mcp",
            "https://peer.internal/mcp",
        ] {
            let config = McpHttpServerConfig {
                endpoint: endpoint.into(),
                ..fixture()
            };
            assert_eq!(
                config.validate(),
                Err("MCP_HTTP_INVALID_ENDPOINT"),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn auth_modes_reject_inline_tokens_and_wrong_references() {
        let mut bearer = fixture();
        bearer.auth = McpHttpAuth::Bearer {
            vault_account: bearer.bearer_vault_account(),
        };
        assert!(bearer.validate().is_ok());
        assert_eq!(
            serde_json::from_str::<McpHttpServerConfig>(
                r#"{"id":"example","endpoint":"https://mcp.example.com/mcp","auth":{"mode":"bearer","token":"plaintext"},"enabled":true,"revision":1}"#
            )
            .err()
            .is_some(),
            true
        );
        bearer.auth = McpHttpAuth::Bearer {
            vault_account: "mcp_env_7_example_API_KEY".into(),
        };
        assert_eq!(bearer.validate(), Err("MCP_HTTP_INVALID_BEARER_REF"));
        bearer.auth = McpHttpAuth::OAuth {
            client_id_metadata_url: Some("http://id.example.com/client".into()),
        };
        assert_eq!(bearer.validate(), Err("MCP_HTTP_INVALID_ENDPOINT"));
    }

    #[test]
    fn disabled_and_revision_changes_invalidate_resolution() {
        let key = [7; 32];
        let mut config = fixture();
        config.auth = McpHttpAuth::Bearer {
            vault_account: config.bearer_vault_account(),
        };
        let resolve = |config: &McpHttpServerConfig, user_id, secret: &'static str| {
            config.resolve_with(&key, user_id, |_| Ok(Zeroizing::new(secret.into())))
        };
        let first = resolve(&config, 3, "first").unwrap();
        assert_eq!(first.bearer.as_deref().map(String::as_str), Some("first"));
        assert_ne!(
            first.effective_revision,
            resolve(&config, 4, "first").unwrap().effective_revision
        );
        assert_ne!(
            first.effective_revision,
            resolve(&config, 3, "second").unwrap().effective_revision
        );
        config.revision = 2;
        assert_ne!(
            first.effective_revision,
            resolve(&config, 3, "first").unwrap().effective_revision
        );
        config.enabled = false;
        assert_eq!(
            resolve(&config, 3, "first").unwrap_err(),
            "MCP_HTTP_DISABLED"
        );
        assert!(!format!("{first:?}").contains("first"));
    }
}
