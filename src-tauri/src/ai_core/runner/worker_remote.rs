//! Exact saved-profile identity check for future workers. Deliberately no
//! connection or remote I/O: the existing temporary-provider factory rereads
//! mutable vault state after authorization, so it is not a worker boundary.
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2024-2026 axpnet: AI-assisted (see AI-TRANSPARENCY.md)

use super::worker_coordinator::{PreparedWorker, WorkerCoordinator};
use super::worker_credentials::ServerPin;

fn exact_server<'a>(
    profile_id: &str,
    servers: &'a [crate::ai_tools::SavedServerInfo],
) -> Result<&'a crate::ai_tools::SavedServerInfo, String> {
    let mut matches = servers.iter().filter(|server| server.id == profile_id);
    let server = matches.next().ok_or("Exact saved profile not found")?;
    if matches.next().is_some() {
        return Err("Duplicate saved profile ID".into());
    }
    Ok(server)
}

/// A pure identity/revision gate. Never return the mutable server record or a
/// connected provider to a child. A later factory must consume one immutable,
/// pinned profile+secret snapshot without calling create_temp_provider.
pub fn validate_exact_profile(
    coordinator: &WorkerCoordinator,
    child: &PreparedWorker,
    profile_id: &str,
) -> Result<(ServerPin, String), String> {
    let (pin, root) = coordinator.resolve_remote(child, profile_id)?;
    let servers = crate::ai_tools::load_saved_servers()?;
    exact_server(&pin.profile_id, &servers)?;
    if coordinator.resolve_remote(child, profile_id)? != (pin.clone(), root.clone()) {
        return Err("Server profile changed during validation".into());
    }
    Ok((pin, root))
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
