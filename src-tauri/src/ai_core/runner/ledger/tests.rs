use super::*;
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

fn limits() -> Limits {
    Limits {
        input_tokens: 100,
        output_tokens: 100,
        requests: 3,
        tool_steps: 3,
        result_bytes: 64,
        concurrent_children: 2,
        deadline: Instant::now() + Duration::from_secs(30),
    }
}

fn cap(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
    }
}

#[test]
fn parent_and_two_siblings_race_for_one_request() {
    let mut l = limits();
    l.requests = 1;
    let ledger = Ledger::new(l);
    let a = ledger.acquire_child(ledger.run_id()).unwrap();
    let b = ledger.acquire_child(ledger.run_id()).unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let mut tasks = vec![];
    for owner in [ledger.run_id().to_string(), a.clone(), b.clone()] {
        let run = ledger.clone();
        let ready = barrier.clone();
        tasks.push(thread::spawn(move || {
            ready.wait();
            run.reserve_request(&owner, cap(10, 10))
        }));
    }
    barrier.wait();
    let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(ledger.snapshot().unwrap().1, 1);
    assert!(ledger.cancellation().is_cancelled());
    for result in results.into_iter().flatten() {
        ledger.finish_request(&result, None).unwrap();
    }
    ledger.finish_child(&a).unwrap();
    ledger.finish_child(&b).unwrap();
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::BudgetExhausted
    );
}

#[test]
fn known_usage_refunds_only_tokens_and_unknown_usage_keeps_reservation() {
    let ledger = Ledger::new(limits());
    let owner = ledger.run_id();
    let first = ledger.reserve_request(owner, cap(20, 20)).unwrap();
    ledger.finish_request(&first, Some(cap(5, 7))).unwrap();
    let retry = ledger.reserve_request(owner, cap(15, 12)).unwrap();
    ledger.finish_request(&retry, None).unwrap();
    assert!(ledger.finish_request(&retry, Some(cap(1, 1))).is_err());
    let (usage, requests, _, _, _) = ledger.snapshot().unwrap();
    assert_eq!(usage, cap(20, 19));
    assert_eq!(requests, 2);
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Completed
    );
}

#[test]
fn usage_over_request_cap_is_recorded_and_exhausts_run() {
    let ledger = Ledger::new(limits());
    let request = ledger
        .reserve_request(ledger.run_id(), cap(10, 10))
        .unwrap();
    assert!(ledger
        .finish_request(&request, Some(cap(13, 14)))
        .unwrap_err()
        .contains("exceeded"));
    assert_eq!(ledger.snapshot().unwrap().0, cap(13, 14));
    assert!(ledger.cancellation().is_cancelled());
    assert!(ledger.reserve_request(ledger.run_id(), cap(1, 1)).is_err());
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::BudgetExhausted
    );
}

#[test]
fn deadline_denies_and_cancels_before_dispatch_with_fake_time() {
    let mut l = limits();
    l.deadline = Instant::now() + Duration::from_secs(1);
    let ledger = Ledger::new(l);
    let error = ledger
        .reserve_request_at(ledger.run_id(), cap(1, 1), l.deadline)
        .unwrap_err();
    assert!(error.contains("deadline"));
    assert_eq!(ledger.snapshot().unwrap().1, 0);
    assert!(ledger.cancellation().is_cancelled());
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::BudgetExhausted
    );
}

#[test]
fn external_token_cancellation_cannot_finish_as_success() {
    let ledger = Ledger::new(limits());
    ledger.cancellation().cancel();
    assert!(ledger.reserve_request(ledger.run_id(), cap(1, 1)).is_err());
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Cancelled
    );
}

#[test]
fn child_depth_slots_and_quiescence_are_enforced() {
    let ledger = Ledger::new(limits());
    let a = ledger.acquire_child(ledger.run_id()).unwrap();
    let b = ledger.acquire_child(ledger.run_id()).unwrap();
    assert!(ledger.acquire_child(&a).unwrap_err().contains("Nested"));
    assert!(ledger
        .acquire_child(ledger.run_id())
        .unwrap_err()
        .contains("concurrency"));
    let step = ledger.reserve_tool_step(&a).unwrap();
    let request = ledger.reserve_request(&a, cap(2, 3)).unwrap();
    ledger.cancel().unwrap();
    assert!(ledger.reserve_tool_step(&b).is_err());
    assert!(ledger.finish_child(&a).unwrap_err().contains("active"));
    assert!(ledger
        .finish(Terminal::Completed)
        .unwrap_err()
        .contains("active"));
    ledger.finish_request(&request, None).unwrap();
    assert!(ledger.finish_child(&a).is_err());
    ledger.finish_tool_step(&step).unwrap();
    ledger.finish_child(&a).unwrap();
    ledger.finish_child(&b).unwrap();
    assert_eq!(
        ledger.finish(Terminal::Completed).unwrap(),
        Terminal::Cancelled
    );
    assert!(ledger.finish(Terminal::Completed).is_err());
}

#[tokio::test]
async fn join_timeout_reports_pending_children_without_releasing_slot() {
    let ledger = Ledger::new(limits());
    let child = ledger.acquire_child(ledger.run_id()).unwrap();
    let status = ledger
        .join(Instant::now() + Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(
        status,
        JoinState::Pending {
            child_ids: vec![child.clone()]
        }
    );
    assert_eq!(ledger.snapshot().unwrap().4, status);
    ledger.cancel().unwrap();
    ledger.finish_child(&child).unwrap();
    ledger.finish(Terminal::Completed).unwrap();
    assert_eq!(
        ledger.join(Instant::now()).await.unwrap(),
        JoinState::Terminal(Terminal::Cancelled)
    );
}

#[tokio::test]
async fn join_wakes_on_terminal_transition() {
    let ledger = Ledger::new(limits());
    let waiting = ledger.clone();
    let waiter = tokio::spawn(async move {
        waiting
            .join(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    ledger.finish(Terminal::Completed).unwrap();
    let result = tokio::time::timeout(Duration::from_millis(200), waiter)
        .await
        .expect("join did not wake")
        .unwrap();
    assert_eq!(result, JoinState::Terminal(Terminal::Completed));
}

#[test]
fn tool_and_result_limits_stop_later_dispatch() {
    let mut l = limits();
    l.tool_steps = 1;
    l.result_bytes = 4;
    let ledger = Ledger::new(l);
    let step = ledger.reserve_tool_step(ledger.run_id()).unwrap();
    ledger.reserve_result_bytes(ledger.run_id(), 4).unwrap();
    assert!(ledger.reserve_tool_step(ledger.run_id()).is_err());
    ledger.finish_tool_step(&step).unwrap();
    assert!(ledger.reserve_result_bytes(ledger.run_id(), 1).is_err());
    assert_eq!(ledger.snapshot().unwrap().2, 1);
    assert_eq!(ledger.snapshot().unwrap().3, 4);
    assert_eq!(
        ledger.finish(Terminal::Failed).unwrap(),
        Terminal::BudgetExhausted
    );
    let ledger = Ledger::new(l);
    assert!(ledger.reserve_result_bytes(ledger.run_id(), 5).is_err());
    assert_eq!(ledger.snapshot().unwrap().3, 0);
}
