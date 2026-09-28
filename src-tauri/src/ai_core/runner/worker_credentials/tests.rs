use super::*;
use crate::ai_core::runner::ledger::{Limits, Terminal};
use std::time::Duration;

struct FakeState {
    model: ModelPin,
    model_key: String,
    server: ServerPin,
    locked: bool,
}

struct FakeSource(Mutex<FakeState>);

impl FakeSource {
    fn new() -> Self {
        Self(Mutex::new(FakeState {
            model: ModelPin {
                provider_id: "provider-1".into(),
                provider_type: "custom".into(),
                endpoint: "https://example.test/v1".into(),
                revision: "model-rev-1".into(),
            },
            model_key: "private-key".into(),
            server: ServerPin {
                profile_id: "server-1".into(),
                revision: "server-rev-1".into(),
            },
            locked: false,
        }))
    }
}

impl WorkerCredentialSource for FakeSource {
    fn model_pin(&self, id: &str) -> Result<ModelPin, String> {
        let state = self.0.lock().unwrap();
        if state.locked || id != state.model.provider_id {
            return Err("locked or unknown".into());
        }
        Ok(state.model.clone())
    }
    fn model_key(&self, id: &str) -> Result<Zeroizing<String>, String> {
        let state = self.0.lock().unwrap();
        if state.locked || id != state.model.provider_id {
            return Err("locked or unknown".into());
        }
        Ok(Zeroizing::new(state.model_key.clone()))
    }
    fn server_pin(&self, id: &str) -> Result<ServerPin, String> {
        let state = self.0.lock().unwrap();
        if state.locked || id != state.server.profile_id {
            return Err("locked or unknown".into());
        }
        Ok(state.server.clone())
    }
}

fn ledger() -> Ledger {
    Ledger::new(Limits {
        input_tokens: 100,
        output_tokens: 100,
        requests: 3,
        tool_steps: 3,
        result_bytes: 1_024,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(30),
    })
}

#[test]
fn exact_ids_reject_aliases_missing_and_duplicates() {
    let records = vec![
        serde_json::json!({"id":"server-1","name":"Production"}),
        serde_json::json!({"id":"server-2","name":"Production"}),
    ];
    assert!(exact_record(&records, "server-1").is_ok());
    for alias in ["", "Active", "Production", "server", " server-1"] {
        assert!(exact_record(&records, alias).is_err());
    }
    let duplicated = vec![records[0].clone(), records[0].clone()];
    assert!(exact_record(&duplicated, "server-1").is_err());
}

#[test]
fn handles_are_child_bound_expire_and_revoke() {
    let ledger = ledger();
    let child_a = ledger.acquire_child(ledger.run_id()).unwrap();
    let child_b = ledger.acquire_child(ledger.run_id()).unwrap();
    let source = Arc::new(FakeSource::new());
    let broker = WorkerCredentialBroker::new(ledger.clone(), source);
    let expires = Instant::now() + Duration::from_secs(5);
    let model = broker.issue_model(&child_a, "provider-1", expires).unwrap();
    let server = broker.issue_server(&child_a, "server-1", expires).unwrap();
    assert_eq!(
        broker.resolve_model(&child_a, &model).unwrap().1.as_str(),
        "private-key"
    );
    assert_eq!(
        broker.resolve_server(&child_a, &server).unwrap().profile_id,
        "server-1"
    );
    assert!(broker.resolve_model(&child_b, &model).is_err());
    assert!(broker.resolve_server(&child_b, &server).is_err());
    assert!(broker.resolve_server(&child_a, &model).is_err());
    let expired = broker
        .issue_model(&child_a, "provider-1", Instant::now())
        .unwrap();
    assert!(broker.resolve_model(&child_a, &expired).is_err());
    broker.revoke_child(&child_a).unwrap();
    assert!(broker.resolve_model(&child_a, &model).is_err());
    ledger.finish_child(&child_a).unwrap();
    ledger.finish_child(&child_b).unwrap();
    ledger.finish(Terminal::Completed).unwrap();
}

#[test]
fn profile_edit_key_rotation_vault_lock_and_cancel_fail_closed() {
    let ledger = ledger();
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    let source = Arc::new(FakeSource::new());
    let broker = WorkerCredentialBroker::new(ledger.clone(), source.clone());
    let expires = Instant::now() + Duration::from_secs(5);
    let model = broker.issue_model(&child, "provider-1", expires).unwrap();
    let server = broker.issue_server(&child, "server-1", expires).unwrap();
    source.0.lock().unwrap().model.revision = "rotated-key-rev".into();
    assert!(broker.resolve_model(&child, &model).is_err());
    source.0.lock().unwrap().server.revision = "edited-profile-rev".into();
    assert!(broker.resolve_server(&child, &server).is_err());
    source.0.lock().unwrap().locked = true;
    assert!(broker.issue_model(&child, "provider-1", expires).is_err());
    assert!(broker.resolve_model(&child, &model).is_err());
    ledger.cancel().unwrap();
    assert!(broker.resolve_server(&child, &server).is_err());
    ledger.finish_child(&child).unwrap();
}
