//! Active-user storage for outbound MCP server settings. No process starts here.

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use rusqlite::Connection;
use serde_json::Value;
use tauri::AppHandle;
use zeroize::Zeroizing;

use crate::credential_store::CredentialStore;
use crate::mcp_client_config::McpServerConfig;
use crate::user_partitions;

const SETTING_SCOPE: &str = "aeroagent_mcp_servers";
const MAX_SERVERS: usize = 32;

pub(crate) fn context(
    app: &AppHandle,
) -> Result<(Connection, Zeroizing<[u8; 32]>, i64), &'static str> {
    user_partitions::init_or_migrate(app).map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    let store = CredentialStore::from_cache().ok_or("MCP_STORE_UNAVAILABLE")?;
    let root_key = Zeroizing::new(store.derive_user_partition_wrapping_key());
    let conn = user_partitions::open_or_init(app).map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    let user_id = user_partitions::active_user_id(&conn)
        .map_err(|_| "MCP_USER_UNAVAILABLE")?
        .ok_or("MCP_USER_UNAVAILABLE")?;
    Ok((conn, root_key, user_id))
}

fn validate_catalog(configs: &[McpServerConfig]) -> Result<(), &'static str> {
    if configs.len() > MAX_SERVERS {
        return Err("MCP_CONFIG_TOO_MANY_SERVERS");
    }
    let mut ids = std::collections::HashSet::new();
    for config in configs {
        config.validate()?;
        if !ids.insert(config.id.as_str()) {
            return Err("MCP_CONFIG_DUPLICATE_ID");
        }
    }
    Ok(())
}

pub(crate) fn load(
    conn: &Connection,
    root_key: &[u8; 32],
    user_id: i64,
) -> Result<Vec<McpServerConfig>, &'static str> {
    let value = user_partitions::get_user_setting_for(conn, root_key, user_id, SETTING_SCOPE)
        .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    let configs = match value {
        Some(value) => serde_json::from_value(value).map_err(|_| "MCP_CONFIG_INVALID_STORED")?,
        None => Vec::new(),
    };
    validate_catalog(&configs)?;
    Ok(configs)
}

fn save(
    conn: &Connection,
    root_key: &[u8; 32],
    user_id: i64,
    configs: &[McpServerConfig],
) -> Result<(), &'static str> {
    validate_catalog(configs)?;
    let value: Value = serde_json::to_value(configs).map_err(|_| "MCP_CONFIG_SERIALIZE_FAILED")?;
    user_partitions::set_user_setting_for(conn, root_key, user_id, SETTING_SCOPE, &value)
        .map_err(|_| "MCP_STORE_UNAVAILABLE")
}

fn upsert_catalog(
    configs: &mut Vec<McpServerConfig>,
    config: McpServerConfig,
) -> Result<Vec<String>, &'static str> {
    config.validate()?;
    let old = configs.iter().position(|item| item.id == config.id);
    let stale_accounts = if let Some(index) = old {
        if config.revision <= configs[index].revision {
            return Err("MCP_CONFIG_STALE_REVISION");
        }
        let stale = configs[index]
            .env
            .values()
            .filter(|old_ref| {
                !config
                    .env
                    .values()
                    .any(|new_ref| new_ref.vault_account == old_ref.vault_account)
            })
            .map(|secret_ref| secret_ref.vault_account.clone())
            .collect();
        configs[index] = config;
        stale
    } else {
        if config.revision != 1 {
            return Err("MCP_CONFIG_INITIAL_REVISION");
        }
        configs.push(config);
        Vec::new()
    };
    configs.sort_by(|a, b| a.id.cmp(&b.id));
    validate_catalog(configs)?;
    Ok(stale_accounts)
}

#[tauri::command]
pub fn mcp_client_list_servers(app: AppHandle) -> Result<Vec<McpServerConfig>, &'static str> {
    let (conn, root_key, user_id) = context(&app)?;
    load(&conn, &root_key, user_id)
}

#[tauri::command]
pub fn mcp_client_upsert_server(
    app: AppHandle,
    config: McpServerConfig,
) -> Result<(), &'static str> {
    let (conn, root_key, user_id) = context(&app)?;
    let mut configs = load(&conn, &root_key, user_id)?;
    let stale_accounts = upsert_catalog(&mut configs, config)?;
    let transaction = conn
        .unchecked_transaction()
        .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    for account in stale_accounts {
        user_partitions::delete_user_credential_for(&transaction, user_id, &account)
            .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    }
    save(&transaction, &root_key, user_id, &configs)?;
    transaction.commit().map_err(|_| "MCP_STORE_UNAVAILABLE")
}

#[tauri::command]
pub fn mcp_client_remove_server(app: AppHandle, server_id: String) -> Result<(), &'static str> {
    let (conn, root_key, user_id) = context(&app)?;
    let mut configs = load(&conn, &root_key, user_id)?;
    let index = configs
        .iter()
        .position(|item| item.id == server_id)
        .ok_or("MCP_CONFIG_NOT_FOUND")?;
    let removed = configs.remove(index);
    let transaction = conn
        .unchecked_transaction()
        .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    for secret_ref in removed.env.values() {
        user_partitions::delete_user_credential_for(
            &transaction,
            user_id,
            &secret_ref.vault_account,
        )
        .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    }
    save(&transaction, &root_key, user_id, &configs)?;
    transaction.commit().map_err(|_| "MCP_STORE_UNAVAILABLE")
}

#[tauri::command]
pub fn mcp_client_set_secret(
    app: AppHandle,
    server_id: String,
    env_name: String,
    secret: String,
) -> Result<(), &'static str> {
    let secret = Zeroizing::new(secret);
    let (conn, root_key, user_id) = context(&app)?;
    let mut configs = load(&conn, &root_key, user_id)?;
    let config = configs
        .iter_mut()
        .find(|item| item.id == server_id)
        .ok_or("MCP_CONFIG_NOT_FOUND")?;
    let secret_ref = config
        .env
        .get(&env_name)
        .ok_or("MCP_CONFIG_INVALID_SECRET_REF")?;
    let account = secret_ref.vault_account.clone();
    config.revision = config
        .revision
        .checked_add(1)
        .ok_or("MCP_CONFIG_REVISION_OVERFLOW")?;
    let transaction = conn
        .unchecked_transaction()
        .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    user_partitions::set_user_credential_for(
        &transaction,
        &root_key,
        user_id,
        &account,
        "mcp_env",
        &secret,
    )
    .map_err(|_| "MCP_STORE_UNAVAILABLE")?;
    save(&transaction, &root_key, user_id, &configs)?;
    transaction.commit().map_err(|_| "MCP_STORE_UNAVAILABLE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: false,
            revision: 1,
        }
    }

    #[test]
    fn catalog_rejects_duplicates_and_excessive_servers() {
        assert!(validate_catalog(&[fixture("one"), fixture("two")]).is_ok());
        assert_eq!(
            validate_catalog(&[fixture("same"), fixture("same")]),
            Err("MCP_CONFIG_DUPLICATE_ID")
        );
        let many = (0..33)
            .map(|index| fixture(&format!("s{index}")))
            .collect::<Vec<_>>();
        assert_eq!(validate_catalog(&many), Err("MCP_CONFIG_TOO_MANY_SERVERS"));
    }

    #[test]
    fn upsert_requires_fresh_revisions_and_identifies_removed_secret_refs() {
        let mut configs = Vec::new();
        assert!(upsert_catalog(&mut configs, fixture("one"))
            .unwrap()
            .is_empty());
        assert_eq!(
            upsert_catalog(&mut configs, fixture("one")),
            Err("MCP_CONFIG_STALE_REVISION")
        );
        let mut changed = fixture("one");
        changed.revision = 2;
        changed.env.insert(
            "API_KEY".into(),
            crate::mcp_client_config::McpSecretRef {
                vault_account: changed.vault_account("API_KEY"),
            },
        );
        assert!(upsert_catalog(&mut configs, changed).unwrap().is_empty());
        let mut removed = fixture("one");
        removed.revision = 3;
        assert_eq!(
            upsert_catalog(&mut configs, removed).unwrap(),
            ["mcp_env_3_one_API_KEY"]
        );
        let mut not_initial = fixture("two");
        not_initial.revision = 2;
        assert_eq!(
            upsert_catalog(&mut configs, not_initial),
            Err("MCP_CONFIG_INITIAL_REVISION")
        );
    }
}
