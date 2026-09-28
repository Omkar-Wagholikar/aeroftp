//! Exact-profile remote connection preparation. No remote operations are
//! exposed until individual provider path confinement is audited.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use crate::providers::StorageProvider;
use std::future::Future;

use super::worker_coordinator::{PreparedWorker, WorkerCoordinator};

pub struct ConnectedProfile {
    profile_id: String,
    root: String,
    _provider: Box<dyn StorageProvider>,
}

impl ConnectedProfile {
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }
    pub fn root(&self) -> &str {
        &self.root
    }
}

fn exact_server(
    profile_id: &str,
    servers: &[crate::ai_tools::SavedServerInfo],
) -> Result<crate::ai_tools::SavedServerInfo, String> {
    let mut matches = servers.iter().filter(|server| server.id == profile_id);
    let server = matches.next().ok_or("Exact saved profile not found")?;
    if matches.next().is_some() {
        return Err("Duplicate saved profile ID".into());
    }
    Ok(server.clone())
}

pub(super) async fn connect_checked<T, F, Fut>(
    coordinator: &WorkerCoordinator,
    child: &PreparedWorker,
    profile_id: &str,
    servers: &[crate::ai_tools::SavedServerInfo],
    connect: F,
) -> Result<(String, String, T), String>
where
    F: FnOnce(crate::ai_tools::SavedServerInfo) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let (pin, root) = coordinator.resolve_remote(child, profile_id)?;
    let server = exact_server(&pin.profile_id, &servers)?;
    let connected = connect(server).await?;
    if coordinator.resolve_remote(child, profile_id)? != (pin.clone(), root.clone()) {
        return Err("Server profile changed while connecting".into());
    }
    Ok((pin.profile_id, root, connected))
}

pub async fn connect_exact(
    coordinator: &WorkerCoordinator,
    child: &PreparedWorker,
    profile_id: &str,
) -> Result<ConnectedProfile, String> {
    let servers = crate::ai_tools::load_saved_servers()?;
    // create_temp_provider reopens the vault and may await a network connect.
    // A changed profile, key, vault or cancelled run must be rejected afterward.
    let (profile_id, root, provider) = connect_checked(
        coordinator,
        child,
        profile_id,
        &servers,
        |server| async move { crate::ai_tools::create_temp_provider(&server).await },
    )
    .await?;
    Ok(ConnectedProfile {
        profile_id,
        root,
        _provider: provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server(id: &str) -> crate::ai_tools::SavedServerInfo {
        crate::ai_tools::SavedServerInfo {
            id: id.into(),
            name: "same name".into(),
            host: "example.test".into(),
            port: 22,
            username: "user".into(),
            protocol: "sftp".into(),
            initial_path: None,
            provider_id: None,
        }
    }
    #[test]
    fn exact_profile_never_selects_alias_or_duplicate() {
        let records = vec![server("one"), server("two")];
        assert_eq!(exact_server("one", &records).unwrap().id, "one");
        for alias in ["", "Active", "same name", "on"] {
            assert!(exact_server(alias, &records).is_err());
        }
        assert!(exact_server("one", &[server("one"), server("one")]).is_err());
    }
}
