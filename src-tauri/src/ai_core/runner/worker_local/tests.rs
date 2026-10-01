use super::*;
use crate::ai_core::runner::ledger::{Limits, Terminal};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn ledger() -> Ledger {
    Ledger::new(Limits {
        input_tokens: 100,
        output_tokens: 100,
        requests: 2,
        tool_steps: 8,
        result_bytes: 1_024,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(30),
    })
}

#[tokio::test]
async fn only_bounded_relative_read_is_dispatched_and_accounted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested").join("evidence.txt"), b"evidence").unwrap();
    let scope = WorkerLocalRead::open_root(dir.path(), 4).unwrap();
    let ledger = ledger();
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    let value = scope
        .dispatch(
            &ledger,
            &child,
            "local_read",
            &json!({"path": "nested/evidence.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(value["content"], "evid");
    assert_eq!(value["size"], 8);
    assert_eq!(value["truncated"], true);
    let (_, _, steps, result_bytes, _) = ledger.snapshot().unwrap();
    assert_eq!(steps, 1);
    assert!(result_bytes > 0 && result_bytes <= 1_024);
    ledger.finish_child(&child).unwrap();
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Completed
    );
}

#[tokio::test]
async fn absolute_traversal_drive_and_mutative_tools_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("safe.txt"), b"safe").unwrap();
    let scope = WorkerLocalRead::open_root(dir.path(), 128).unwrap();
    let ledger = ledger();
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    for path in [
        "../safe.txt",
        "/etc/passwd",
        "C:\\secret",
        "safe.txt/../../x",
    ] {
        assert!(scope
            .dispatch(&ledger, &child, "local_read", &json!({"path": path}))
            .await
            .is_err());
    }
    for tool in ["local_write", "shell_execute", "local_list", "spawn_worker"] {
        assert!(scope
            .dispatch(&ledger, &child, tool, &json!({"path": "safe.txt"}))
            .await
            .unwrap_err()
            .contains("allowlist"));
    }
    assert_eq!(ledger.snapshot().unwrap().2, 8);
    ledger.finish_child(&child).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_swap_after_path_check_cannot_escape_open_root() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"outside-secret").unwrap();
    let link = dir.path().join("link");
    std::fs::create_dir(&link).unwrap();
    let scope = WorkerLocalRead::open_root(dir.path(), 128).unwrap();
    WorkerLocalRead::checked_relative_path("link/secret.txt").unwrap();
    std::fs::remove_dir(&link).unwrap();
    symlink(outside.path(), &link).unwrap();

    let ledger = ledger();
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    assert!(scope
        .dispatch(
            &ledger,
            &child,
            "local_read",
            &json!({"path": "link/secret.txt"})
        )
        .await
        .is_err());
    ledger.finish_child(&child).unwrap();
}

#[tokio::test]
async fn result_budget_denies_publication_and_preserves_quiescence() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), b"larger-result").unwrap();
    let scope = WorkerLocalRead::open_root(dir.path(), 64).unwrap();
    let limits = Limits {
        input_tokens: 100,
        output_tokens: 100,
        requests: 2,
        tool_steps: 1,
        result_bytes: 4,
        concurrent_children: 1,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    let ledger = Ledger::new(limits);
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    assert!(scope
        .dispatch(&ledger, &child, "local_read", &json!({"path": "file.txt"}))
        .await
        .is_err());
    assert_eq!(ledger.snapshot().unwrap().3, 0);
    ledger.finish_child(&child).unwrap();
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::BudgetExhausted
    );
    assert!(WorkerLocalRead::open_root(dir.path(), 0).is_err());
    assert!(WorkerLocalRead::open_root(dir.path(), MAX_RESULT_BYTES + 1).is_err());
}

#[test]
fn cancelled_queued_job_never_opens_file_and_dropped_await_keeps_slot() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"should-not-open").unwrap();
        let scope = WorkerLocalRead::open_root(dir.path(), 64).unwrap();
        let ledger = ledger();
        let child = ledger.acquire_child(ledger.run_id()).unwrap();
        let dispatched = scope.clone();
        let run = ledger.clone();
        let owner = child.clone();
        let task = tokio::spawn(async move {
            dispatched
                .dispatch(&run, &owner, "local_read", &json!({"path": "file.txt"}))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while ledger.snapshot().unwrap().2 == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        ledger.cancel().unwrap();
        task.abort();
        assert!(ledger.finish_child(&child).is_err());

        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while ledger.finish_child(&child).is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(scope.open_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            ledger.finish(Terminal::Completed).unwrap(),
            Terminal::Cancelled
        );
    });
}
