//! Opaque, run-scoped credential handles for future workers. Secrets are read
//! by the trusted coordinator at dispatch, never serialized into WorkerSpec.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde_json::Value;
use sha2::Sha256;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use super::ledger::Ledger;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPin {
    pub provider_id: String,
    pub provider_type: String,
    pub endpoint: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPin {
    pub profile_id: String,
    pub revision: String,
}

/// Backend-only, single-use connection input. It is never serialized or sent
/// to the model. In particular, the network factory must not reload the vault.
pub struct PinnedServerSnapshot {
    pub(crate) pin: ServerPin,
    pub(crate) config: crate::providers::ProviderConfig,
    pub(crate) secret: Zeroizing<String>,
}

impl Drop for PinnedServerSnapshot {
    fn drop(&mut self) {
        self.config.zeroize_password();
        for value in self.config.extra.values_mut() {
            value.zeroize();
        }
    }
}

/// All methods are privileged backend operations. The model sees only tool
/// schemas and bounded evidence, never this trait or its returned secrets.
pub trait WorkerCredentialSource: Send + Sync {
    fn model_pin(&self, provider_id: &str) -> Result<ModelPin, String>;
    fn model_key(&self, provider_id: &str) -> Result<Zeroizing<String>, String>;
    fn server_pin(&self, profile_id: &str) -> Result<ServerPin, String>;
    fn server_snapshot(&self, _profile_id: &str) -> Result<PinnedServerSnapshot, String> {
        Err("Worker remote profile snapshots are unavailable".into())
    }
}

pub struct VaultWorkerCredentialSource;

fn nonempty_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.trim() != id || id.eq_ignore_ascii_case("active") {
        return Err("An exact saved ID is required".into());
    }
    Ok(())
}

fn vault() -> Result<crate::credential_store::CredentialStore, String> {
    crate::credential_store::CredentialStore::from_cache()
        .ok_or_else(|| "Credential vault is locked".into())
}

fn revision_key() -> &'static Zeroizing<[u8; 32]> {
    static KEY: OnceLock<Zeroizing<[u8; 32]>> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = Zeroizing::new([0; 32]);
        OsRng.fill_bytes(&mut *key);
        key
    })
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = Hmac::<Sha256>::new_from_slice(&revision_key()[..])
        .expect("HMAC accepts a 32-byte revision key");
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize().into_bytes())
}

fn exact_record<'a>(records: &'a [Value], id: &str) -> Result<&'a Value, String> {
    nonempty_id(id)?;
    let mut matches = records
        .iter()
        .filter(|record| record.get("id").and_then(Value::as_str) == Some(id));
    let one = matches
        .next()
        .ok_or_else(|| "Exact saved ID not found".to_string())?;
    if matches.next().is_some() {
        return Err("Duplicate saved ID".into());
    }
    Ok(one)
}

fn model_record(
    store: &crate::credential_store::CredentialStore,
    id: &str,
) -> Result<Value, String> {
    let raw = store
        .get("config_ai_settings")
        .or_else(|_| store.get("ai_settings"))
        .map_err(|_| "AI settings unavailable".to_string())?;
    let settings: Value = serde_json::from_str(&raw).map_err(|_| "Invalid AI settings")?;
    let providers = settings
        .get("providers")
        .and_then(Value::as_array)
        .ok_or("AI provider list unavailable")?;
    let provider = exact_record(providers, id)?;
    if provider.get("isEnabled").and_then(Value::as_bool) != Some(true) {
        return Err("AI provider is disabled".into());
    }
    Ok(provider.clone())
}

fn server_record(
    store: &crate::credential_store::CredentialStore,
    id: &str,
) -> Result<Value, String> {
    let raw = store
        .get("config_server_profiles")
        .map_err(|_| "Server profiles unavailable".to_string())?;
    let profiles: Vec<Value> = serde_json::from_str(&raw).map_err(|_| "Invalid server profiles")?;
    Ok(exact_record(&profiles, id)?.clone())
}

fn active_secret(
    store: &crate::credential_store::CredentialStore,
    key: &str,
) -> Result<Zeroizing<String>, String> {
    crate::user_partitions::resolve_active_credential(store, key)?
        .ok_or_else(|| "Credential unavailable".into())
}

fn model_auth_key(
    provider_type: &str,
    load_secret: impl FnOnce() -> Result<Zeroizing<String>, String>,
) -> Result<Zeroizing<String>, String> {
    // The foreground Ollama adapter uses this non-secret sentinel and has no
    // API-key field. Still require the enabled provider record and unlocked
    // vault before reaching this helper.
    if provider_type == "ollama" {
        Ok(Zeroizing::new("ollama".into()))
    } else {
        load_secret()
    }
}

fn snapshot_from_record(
    profile_id: &str,
    profile: &Value,
    secret: Zeroizing<String>,
) -> Result<PinnedServerSnapshot, String> {
    if profile.get("id").and_then(Value::as_str) != Some(profile_id)
        || profile.get("protocol").and_then(Value::as_str) != Some("s3")
        || profile
            .get("aeroCryptOverlay")
            .is_some_and(|value| !value.is_null())
        || profile
            .get("password")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    {
        return Err("Worker remote profile requires plain S3 and a separate credential".into());
    }
    let mut extra = HashMap::new();
    crate::profile_loader::apply_profile_options(&mut extra, profile);
    // STS and provider-specific bridges add external credentials or mutable
    // authority; they need their own reviewed snapshot boundary.
    if extra
        .keys()
        .any(|key| key == "session_token" || key.starts_with("role_"))
        || extra
            .get("provider_id")
            .is_some_and(|id| id.contains("filen"))
        || extra.get("bucket").is_none_or(String::is_empty)
    {
        return Err("S3 worker profile uses an unsupported credential mode".into());
    }
    let encoded = serde_json::to_vec(profile).map_err(|_| "Invalid server profile")?;
    let pin = ServerPin {
        profile_id: profile_id.into(),
        revision: digest(&[&encoded, secret.as_bytes()]),
    };
    let host = profile
        .get("server")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| profile.get("host").and_then(Value::as_str))
        .ok_or("S3 worker endpoint missing")?;
    let (host, embedded_port) = crate::cloud_provider_factory::parse_server_field(host);
    let port = profile
        .get("port")
        .and_then(Value::as_u64)
        .filter(|value| (1..=u16::MAX as u64).contains(value))
        .map(|value| value as u16);
    let config = crate::providers::ProviderConfig {
        name: profile
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("S3")
            .into(),
        provider_type: crate::providers::ProviderType::S3,
        host,
        port: embedded_port.or(port),
        username: profile
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_owned),
        password: None,
        initial_path: profile
            .get("initialPath")
            .and_then(Value::as_str)
            .map(str::to_owned),
        extra,
    };
    Ok(PinnedServerSnapshot {
        pin,
        config,
        secret,
    })
}

impl WorkerCredentialSource for VaultWorkerCredentialSource {
    fn model_pin(&self, provider_id: &str) -> Result<ModelPin, String> {
        let store = vault()?;
        let record = model_record(&store, provider_id)?;
        let provider_type = record
            .get("type")
            .and_then(Value::as_str)
            .ok_or("AI provider type missing")?;
        let endpoint = record
            .get("baseUrl")
            .and_then(Value::as_str)
            .ok_or("AI provider endpoint missing")?;
        if endpoint.is_empty() {
            return Err("AI provider endpoint missing".into());
        }
        let secret = model_auth_key(provider_type, || {
            active_secret(&store, &format!("ai_apikey_{provider_id}"))
        })?;
        let encoded = serde_json::to_vec(&record).map_err(|_| "Invalid AI provider record")?;
        Ok(ModelPin {
            provider_id: provider_id.into(),
            provider_type: provider_type.into(),
            endpoint: endpoint.into(),
            revision: digest(&[&encoded, secret.as_bytes()]),
        })
    }

    fn model_key(&self, provider_id: &str) -> Result<Zeroizing<String>, String> {
        let store = vault()?;
        let record = model_record(&store, provider_id)?;
        let provider_type = record
            .get("type")
            .and_then(Value::as_str)
            .ok_or("AI provider type missing")?;
        model_auth_key(provider_type, || {
            active_secret(&store, &format!("ai_apikey_{provider_id}"))
        })
    }

    fn server_pin(&self, profile_id: &str) -> Result<ServerPin, String> {
        let store = vault()?;
        let profile = server_record(&store, profile_id)?;
        let encoded = serde_json::to_vec(&profile).map_err(|_| "Invalid server profile")?;
        let secret = crate::user_partitions::resolve_active_credential(
            &store,
            &format!("server_{profile_id}"),
        )?;
        let secret_bytes = secret.as_ref().map(|s| s.as_bytes()).unwrap_or_default();
        Ok(ServerPin {
            profile_id: profile_id.into(),
            revision: digest(&[&encoded, secret_bytes]),
        })
    }

    fn server_snapshot(&self, profile_id: &str) -> Result<PinnedServerSnapshot, String> {
        let store = vault()?;
        let profile = server_record(&store, profile_id)?;
        let secret = active_secret(&store, &format!("server_{profile_id}"))?;
        snapshot_from_record(profile_id, &profile, secret)
    }
}

#[derive(Clone)]
enum HandleKind {
    Model(ModelPin),
    Server(ServerPin),
}

#[derive(Clone)]
struct HandleEntry {
    child_id: String,
    expires: Instant,
    kind: HandleKind,
}

/// A handle ID is only an opaque reference. Every use checks run state,
/// child identity, expiry and the current vault revision again.
pub struct WorkerCredentialBroker {
    ledger: Ledger,
    source: Arc<dyn WorkerCredentialSource>,
    handles: Mutex<HashMap<String, HandleEntry>>,
}

impl WorkerCredentialBroker {
    pub fn new(ledger: Ledger, source: Arc<dyn WorkerCredentialSource>) -> Self {
        Self {
            ledger,
            source,
            handles: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, child_id: &str, expires: Instant, kind: HandleKind) -> Result<String, String> {
        if !self.ledger.child_active(child_id)? || self.ledger.cancellation().is_cancelled() {
            return Err("Worker child is inactive".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.handles
            .lock()
            .map_err(|_| "Credential handle lock poisoned")?
            .insert(
                id.clone(),
                HandleEntry {
                    child_id: child_id.into(),
                    expires,
                    kind,
                },
            );
        Ok(id)
    }

    pub fn issue_model(
        &self,
        child_id: &str,
        provider_id: &str,
        expires: Instant,
    ) -> Result<String, String> {
        let pin = self.source.model_pin(provider_id)?;
        self.insert(child_id, expires, HandleKind::Model(pin))
    }

    pub fn issue_model_pinned(
        &self,
        child_id: &str,
        pin: &ModelPin,
        expires: Instant,
    ) -> Result<String, String> {
        if self.source.model_pin(&pin.provider_id)? != *pin {
            return Err("AI provider changed before worker preparation".into());
        }
        self.insert(child_id, expires, HandleKind::Model(pin.clone()))
    }

    pub fn issue_server(
        &self,
        child_id: &str,
        profile_id: &str,
        expires: Instant,
    ) -> Result<String, String> {
        let pin = self.source.server_pin(profile_id)?;
        self.insert(child_id, expires, HandleKind::Server(pin))
    }

    pub fn issue_server_pinned(
        &self,
        child_id: &str,
        pin: &ServerPin,
        expires: Instant,
    ) -> Result<String, String> {
        if self.source.server_pin(&pin.profile_id)? != *pin {
            return Err("Server profile changed before worker preparation".into());
        }
        self.insert(child_id, expires, HandleKind::Server(pin.clone()))
    }

    fn entry(&self, child_id: &str, handle_id: &str) -> Result<HandleEntry, String> {
        if self.ledger.cancellation().is_cancelled() || !self.ledger.child_active(child_id)? {
            return Err("Worker child is inactive".into());
        }
        let entry = self
            .handles
            .lock()
            .map_err(|_| "Credential handle lock poisoned")?
            .get(handle_id)
            .cloned()
            .ok_or("Unknown or revoked credential handle")?;
        if entry.child_id != child_id || Instant::now() >= entry.expires {
            return Err("Credential handle expired or belongs to another child".into());
        }
        Ok(entry)
    }

    pub fn resolve_model(
        &self,
        child_id: &str,
        handle_id: &str,
    ) -> Result<(ModelPin, Zeroizing<String>), String> {
        let HandleKind::Model(pin) = self.entry(child_id, handle_id)?.kind else {
            return Err("Credential handle type mismatch".into());
        };
        if self.source.model_pin(&pin.provider_id)? != pin {
            return Err("AI provider changed since worker preparation".into());
        }
        let secret = self.source.model_key(&pin.provider_id)?;
        if self.source.model_pin(&pin.provider_id)? != pin
            || self.ledger.cancellation().is_cancelled()
        {
            return Err("AI provider changed before dispatch".into());
        }
        Ok((pin, secret))
    }

    pub fn resolve_server(&self, child_id: &str, handle_id: &str) -> Result<ServerPin, String> {
        let HandleKind::Server(pin) = self.entry(child_id, handle_id)?.kind else {
            return Err("Credential handle type mismatch".into());
        };
        if self.source.server_pin(&pin.profile_id)? != pin {
            return Err("Server profile changed since worker preparation".into());
        }
        Ok(pin)
    }

    pub fn resolve_server_snapshot(
        &self,
        child_id: &str,
        handle_id: &str,
    ) -> Result<PinnedServerSnapshot, String> {
        let HandleKind::Server(pin) = self.entry(child_id, handle_id)?.kind else {
            return Err("Credential handle type mismatch".into());
        };
        let snapshot = self.source.server_snapshot(&pin.profile_id)?;
        if snapshot.pin != pin
            || self.source.server_pin(&pin.profile_id)? != pin
            || self.entry(child_id, handle_id).is_err()
        {
            return Err("Server profile changed before worker dispatch".into());
        }
        Ok(snapshot)
    }

    pub fn revoke_child(&self, child_id: &str) -> Result<(), String> {
        self.handles
            .lock()
            .map_err(|_| "Credential handle lock poisoned")?
            .retain(|_, entry| entry.child_id != child_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
