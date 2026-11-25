use std::sync::{Barrier, mpsc};
use std::thread;

use serde_json::json;

use super::*;
use crate::{Hub, test_support::TestDir};

fn request() -> Registration {
    Registration {
        owner: ApprovalOwner {
            thread: "thread-1".into(),
            job: "job-1".into(),
            requester: "worker-1".into(),
        },
        risk: ApprovalRisk::Mid,
        unattended: ApprovalUnattended::DeferAndContinue,
        reason: "Edit requested".into(),
        expires_at: now() + 60_000,
        action: Action {
            tool: "fixture_write".into(),
            args: json!({"secret": "fixture-secret", "path": "chapter.md"}),
            scope: json!({"checkout": "work-1", "before": "abc"}),
        },
        display_args: json!({"secret": "[redacted]", "path": "chapter.md"}),
        display_scope: json!({"checkout": "work-1"}),
    }
}

fn hub(dir: &Path) -> Hub {
    let hub = Hub::new();
    hub.init_approval_queue("workspace".into(), dir);
    hub
}

#[test]
fn full_approval_grants_one_claim_and_masks_its_history() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    let request = || {
        let mut request = request();
        request.action.tool = "fs_write".into();
        request
    };
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::Human);

    let change = FullApprovalChange::Full {
        duration: FullApprovalDuration::OneHour,
    };
    let enabled = queue.set_full_mode(change, "test-user", vec![], vec![])?.0.unwrap();
    assert_eq!(enabled.expires_at, Some(enabled.enabled_at + HOUR_MS));
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::Granted);
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::AlreadyConsumed);

    let mut push = request();
    push.action.tool = "git push".into();
    push.unattended = ApprovalUnattended::NeverUnattended;
    let mut desk_push = request();
    desk_push.owner = crate::full_approval::desk_owner("thread-1");
    desk_push.action.tool = "git push".into();
    desk_push.unattended = ApprovalUnattended::NeverUnattended;
    assert_eq!(queue.authorize_full(push)?, FullAuthorization::Granted);
    let pending = queue.register(desk_push)?;
    assert_eq!(queue.pending()?.0[0].approval_id, pending);
    let audit = queue.full_settings(None)?;
    assert_eq!(audit.audit.len(), 2);
    assert!(audit.audit.iter().all(|entry| entry.granted_by == "full_mode" && entry.action.args == json!("[redacted]")));
    assert_eq!(audit.last_24_hours_by_tool["fs_write"], 1);

    let mut unclaimed = request();
    unclaimed.action.args = json!({"other": "contents"});
    let owner = unclaimed.owner.clone();
    let action = unclaimed.action.clone();
    let id = queue.register(unclaimed)?;
    queue.set_full_mode(FullApprovalChange::Off, "test-user", vec![], vec![])?;
    assert!(queue.consume_once(&id, &owner, &action)?);
    assert!(!queue.consume_once(&id, &owner, &action)?);
    assert!(queue.full_settings(None)?.active.is_none());
    Ok(())
}

#[test]
fn full_approval_expiry_and_until_disabled() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    let change = FullApprovalChange::Full {
        duration: FullApprovalDuration::EightHours,
    };
    queue.set_full_mode_at(change, "test-user", || 0, vec![], vec![])?;
    queue.with_store(|| 8 * HOUR_MS - 1, |store, now| {
        assert!(store.active_mode(now).is_some());
        assert!(store.active_mode(now + 1).is_none());
        Ok(())
    })?;

    let until_disabled = queue.set_full_mode(FullApprovalChange::Full {
        duration: FullApprovalDuration::UntilDisabled,
    }, "test-user", vec![], vec![])?.0.unwrap();
    assert_eq!(until_disabled.expires_at, None);
    assert!(crate::full_approval::command(&hub(dir.path()), "8h", None).is_err());
    assert!(queue.full_settings(None)?.active.is_some());
    Ok(())
}

const GRANT_ONCE: ApprovalDecision = ApprovalDecision::Grant {
    scope: ApprovalGrantScope::Once,
};
const ENABLE_FULL: FullApprovalChange = FullApprovalChange::Full {
    duration: FullApprovalDuration::UntilDisabled,
};

#[test]
fn full_sweep_and_human_decision_record_one_winner() -> Result<()> {
    for _ in 0..8 {
        let dir = TestDir::new();
        let queue = Queue::new("workspace".into(), dir.path());
        let registration = request();
        let (owner, action) = (registration.owner.clone(), registration.action.clone());
        let id = queue.register(registration)?;
        let (_, mut updates) = queue.pending()?;
        let barrier = Barrier::new(2);

        let denied = thread::scope(|scope| -> Result<_> {
            let human = scope.spawn(|| {
                barrier.wait();
                queue.decide(&id, ApprovalDecision::Deny, "desktop", None)
            });
            barrier.wait();
            queue.set_full_mode(ENABLE_FULL, "desktop", vec![], vec![])?;
            Ok(human.join().unwrap().is_ok())
        })?;
        let decision = updates.try_recv()?;
        assert_eq!(decision.approval_id, id);
        assert!(updates.try_recv().is_err());
        assert!(queue.pending()?.0.is_empty());
        if denied {
            assert_eq!(decision.state, ApprovalState::Denied);
            assert!(queue.full_settings(None)?.audit.is_empty());
            assert!(!queue.consume_once(&id, &owner, &action)?);
        } else {
            assert_eq!(decision.state, ApprovalState::Granted);
            assert_eq!(queue.full_settings(None)?.audit[0].approval_id, id);
            assert!(queue.consume_once(&id, &owner, &action)?);
            assert!(!queue.consume_once(&id, &owner, &action)?);
        }
        let saved = queue.load()?;
        assert_eq!(saved.records[0].history.iter().filter(|entry| {
            matches!(entry.event, AuditEvent::Granted { .. } | AuditEvent::Denied)
        }).count(), 1);
    }
    Ok(())
}

#[test]
fn full_approval_restores_and_sweeps_queue_from_previous_server() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    let eligible = queue.register(request())?;
    let mut control_plane = request();
    control_plane.owner = crate::full_approval::desk_owner("thread-1");
    control_plane.action.tool = "self_update".into();
    let human = queue.register(control_plane)?;
    queue.write_json(FULL_MODE_FILE, &Some(ActiveMode {
        id: "saved-mode".into(),
        settings: FullApprovalMode {
            duration: FullApprovalDuration::UntilDisabled,
            enabled_at: now(),
            expires_at: None,
            enabled_by: "desktop".into(),
        },
    }))?;
    drop(queue);

    let restarted = hub(dir.path());
    restarted.restore_full_approval()?;
    let settings = restarted.full_approval_settings(None)?;
    assert_eq!(settings.active.unwrap().enabled_by, "desktop");
    assert_eq!(settings.audit.len(), 1);
    assert_eq!(settings.audit[0].approval_id, eligible);
    let pending = restarted.pending_approvals()?.0;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].approval_id, human);
    restarted.set_full_approval(FullApprovalChange::Off, &crate::remote::SettingsPrincipal::fixture())?;
    let request = request();
    assert!(restarted.consume_once_approval(&eligible, &request.owner, &request.action)?);
    assert!(!restarted.consume_once_approval(&eligible, &request.owner, &request.action)?);
    drop(restarted);

    let restarted = hub(dir.path());
    assert!(restarted.full_approval_settings(None)?.active.is_none());
    assert_eq!(restarted.full_approval_settings(None)?.audit.len(), 1);
    Ok(())
}

#[test]
fn consumed_full_approval_is_idempotent_across_toggles_and_restart() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    queue.set_full_mode(ENABLE_FULL, "desktop", vec![], vec![])?;
    let id = queue.register(request())?;
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::Granted);
    queue.set_full_mode(FullApprovalChange::Off, "desktop", vec![], vec![])?;
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::AlreadyConsumed);
    queue.set_full_mode(ENABLE_FULL, "desktop", vec![], vec![])?;
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::AlreadyConsumed);
    drop(queue);

    let queue = Queue::new("workspace".into(), dir.path());
    assert!(queue.full_settings(None)?.active.is_some());
    assert_eq!(queue.register(request())?, id);
    assert_eq!(queue.authorize_full(request())?, FullAuthorization::AlreadyConsumed);
    let store = queue.load()?;
    assert_eq!(store.records.len(), 1);
    let events = &store.records[0].history;
    assert_eq!(events.iter().filter(|entry| matches!(entry.event, AuditEvent::Granted { .. })).count(), 1);
    assert_eq!(events.iter().filter(|entry| matches!(entry.event, AuditEvent::Consumed)).count(), 1);
    assert_eq!(queue.full_settings(None)?.audit[0].approval_id, id);
    Ok(())
}

#[test]
fn earlier_leases_with_duplicate_full_grants_cannot_run_twice() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    queue.set_full_mode(ENABLE_FULL, "desktop", vec![], vec![])?;
    let first = queue.register(request())?;
    let replay = "legacy-duplicate";
    queue.with_store(now, |store, at| {
        let mut next = store.clone();
        let mut duplicate = queue.new_record(replay.into(), request(), at);
        duplicate.grant_full(at, store.active_mode(at).unwrap());
        duplicate.consume(at);
        next.records.push(duplicate);
        queue.commit(store, next, [])
    })?;
    drop(queue);

    let queue = Queue::new("workspace".into(), dir.path());
    let request = request();
    assert!(!queue.consume_once(&first, &request.owner, &request.action)?);
    assert!(!queue.consume_once(replay, &request.owner, &request.action)?);
    assert_eq!(queue.authorize_full(request)?, FullAuthorization::AlreadyConsumed);
    Ok(())
}

#[test]
fn concurrent_registration_deduplicates_and_notifies_once() -> Result<()> {
    let dir = TestDir::new();
    let hub = hub(dir.path());
    let (pending, mut updates) = hub.pending_approvals()?;
    assert!(pending.is_empty());
    let barrier = Barrier::new(8);

    let ids = thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|i| {
                let (hub, barrier) = (&hub, &barrier);
                scope.spawn(move || {
                    let mut registration = request();
                    if i % 2 == 0 {
                        registration.action.args = serde_json::from_str(
                            r#"{ "path": "chapter.md", "secret": "fixture-secret" }"#,
                        )?;
                    }
                    barrier.wait();
                    hub.register_approval(registration)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Result<Vec<_>>>()
    })?;

    let first = updates.try_recv()?;
    assert!(first.approval_id.starts_with("apr_"));
    assert!(ids.iter().all(|id| *id == first.approval_id));
    assert_eq!(first.action.args["secret"], "[redacted]");
    assert!(!serde_json::to_string(&first)?.contains("fixture-secret"));
    assert!(matches!(
        updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(hub.pending_approvals()?.0, [first]);

    let mut changed = request();
    changed.action.scope["before"] = json!("new-hash");
    assert_ne!(hub.register_approval(changed)?, ids[0]);
    let mut changed = request();
    changed.owner.job = "job-2".into();
    assert_ne!(hub.register_approval(changed)?, ids[0]);
    let saved: Store = serde_json::from_slice(&fs::read(dir.path().join(FILE))?)?;
    assert_eq!(saved.records[0].history.len(), ids.len());
    assert!(matches!(
        saved.records[0].history[0].event,
        AuditEvent::Registered
    ));
    assert!(
        saved.records[0].history[1..]
            .iter()
            .all(|entry| matches!(entry.event, AuditEvent::Deduplicated))
    );
    Ok(())
}

#[test]
fn only_one_matching_retry_consumes_a_once_grant() -> Result<()> {
    let dir = TestDir::new();
    let hub = hub(dir.path());
    let registration = request();
    let owner = registration.owner.clone();
    let action = registration.action.clone();
    let id = hub.register_approval(registration)?;
    assert!(!hub.consume_once_approval(&id, &owner, &action)?);
    hub.decide_approval(&id, GRANT_ONCE, "desktop", None)?;
    let mut different = action.clone();
    different.args["path"] = json!("another.md");
    assert!(!hub.consume_once_approval(&id, &owner, &different)?);
    let mut other = owner.clone();
    other.requester = "different-worker".into();
    assert!(!hub.consume_once_approval(&id, &other, &action)?);

    let barrier = Barrier::new(8);
    let successes = thread::scope(|scope| {
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    hub.consume_once_approval(&id, &owner, &action)
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Result<Vec<_>>>()
    })?;
    assert_eq!(
        successes.into_iter().filter(|consumed| *consumed).count(),
        1
    );
    assert!(
        hub.decide_approval(&id, ApprovalDecision::Withdraw, "android", None)
            .is_err()
    );
    Ok(())
}

#[test]
fn expiry_and_consumption_are_serialized_at_the_deadline() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    let request = request();
    let (owner, action, deadline) = (
        request.owner.clone(),
        request.action.clone(),
        request.expires_at,
    );
    let id = queue.register(request)?;
    queue.decide(&id, GRANT_ONCE, "desktop", None)?;
    let (_, mut updates) = queue.pending()?;

    // Pause a successful claim under the queue lock while expiry tries to acquire it.
    let (locked, lock_acquired) = mpsc::sync_channel(0);
    let (expiring, expiry_started) = mpsc::sync_channel(0);
    thread::scope(|scope| -> Result<()> {
        let consume = scope.spawn(|| {
            queue.consume_once_at(&id, &owner, &action, move || {
                locked.send(()).unwrap();
                expiry_started.recv().unwrap();
                deadline - 1
            })
        });
        lock_acquired.recv()?;
        let expire = scope.spawn(|| {
            expiring.send(()).unwrap();
            queue.expire_at(|| deadline)
        });
        assert!(consume.join().unwrap()?);
        expire.join().unwrap()
    })?;
    let consumed = updates.try_recv()?;
    assert_eq!(consumed.state, ApprovalState::Granted);
    assert_eq!(consumed.consumed_at, Some(deadline - 1));
    assert!(matches!(
        updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // At the deadline, either racing call can expire the request, and neither can claim it.
    let mut registration = self::request();
    registration.expires_at = deadline;
    let id = queue.register(registration)?;
    queue.decide(&id, GRANT_ONCE, "desktop", None)?;
    let (_, mut updates) = queue.pending()?;
    let barrier = Barrier::new(2);
    thread::scope(|scope| -> Result<()> {
        let consume = scope.spawn(|| {
            barrier.wait();
            queue.consume_once_at(&id, &owner, &action, || deadline)
        });
        barrier.wait();
        queue.expire_at(|| deadline)?;
        assert!(!consume.join().unwrap()?);
        Ok(())
    })?;
    let expired = updates.try_recv()?;
    assert_eq!(expired.state, ApprovalState::Expired);
    assert_eq!(expired.consumed_at, None);
    assert!(matches!(
        updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn restart_preserves_pending_decisions_and_consumption() -> Result<()> {
    let dir = TestDir::new();
    let first = hub(dir.path());
    let registration = request();
    let (owner, action) = (registration.owner.clone(), registration.action.clone());
    let used = first.register_approval(registration)?;
    first.decide_approval(&used, GRANT_ONCE, "device:desktop", Some("reviewed".into()))?;
    assert!(first.consume_once_approval(&used, &owner, &action)?);
    let pending = first.register_approval(request())?;
    drop(first);

    let restarted = hub(dir.path());
    let (snapshot, _) = restarted.pending_approvals()?;
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].approval_id, pending);
    assert_eq!(restarted.register_approval(request())?, pending);
    assert!(!restarted.consume_once_approval(&used, &owner, &action)?);
    assert!(
        restarted
            .decide_approval(&used, GRANT_ONCE, "device:android", None)
            .is_err()
    );

    let saved: Store = serde_json::from_slice(&fs::read(dir.path().join(FILE))?)?;
    let history = &saved.records[0].history;
    assert_eq!(history.len(), 3);
    assert!(matches!(
        history[1].event,
        AuditEvent::Granted {
            scope: ApprovalGrantScope::Once,
            full_mode_id: None,
        }
    ));
    assert_eq!(history[1].principal, "device:desktop");
    assert_eq!(history[1].reason.as_deref(), Some("reviewed"));
    assert!(matches!(history[2].event, AuditEvent::Consumed));
    Ok(())
}

#[test]
fn denied_withdrawn_and_expired_requests_cannot_be_granted_or_consumed() -> Result<()> {
    let dir = TestDir::new();
    let hub = hub(dir.path());
    let template = request();
    let (owner, action, deadline) = (
        template.owner.clone(),
        template.action.clone(),
        template.expires_at,
    );
    let denied = hub.register_approval(template)?;
    assert_eq!(
        hub.decide_approval(&denied, ApprovalDecision::Deny, "android", None)?
            .state,
        ApprovalState::Denied
    );
    let withdrawn = hub.register_approval(request())?;
    hub.decide_approval(&withdrawn, GRANT_ONCE, "desktop", None)?;
    let mut different_owner = owner.clone();
    different_owner.job = "another-job".into();
    assert!(
        hub.withdraw_own_approval(&withdrawn, &different_owner, None)
            .is_err()
    );
    assert_eq!(
        hub.withdraw_own_approval(&withdrawn, &owner, None)?.state,
        ApprovalState::Withdrawn
    );
    let expired = hub.register_approval(request())?;
    hub.approval_queue
        .get()
        .unwrap()
        .expire_at(|| deadline + 60_000)?;

    for id in [denied, withdrawn, expired] {
        assert!(
            hub.decide_approval(&id, GRANT_ONCE, "desktop", None)
                .is_err()
        );
        assert!(!hub.consume_once_approval(&id, &owner, &action)?);
    }
    assert!(hub.pending_approvals()?.0.is_empty());
    Ok(())
}

#[test]
fn scoped_and_time_limited_decisions_use_their_explicit_deadlines() -> Result<()> {
    let dir = TestDir::new();
    let queue = Queue::new("workspace".into(), dir.path());
    let deadline = now() + 120_000;
    for scope in [
        ApprovalGrantScope::Scoped {
            expires_at: deadline,
        },
        ApprovalGrantScope::TimeLimited {
            expires_at: deadline,
        },
    ] {
        let registration = request();
        let (owner, action) = (registration.owner.clone(), registration.action.clone());
        let id = queue.register(registration)?;
        assert_eq!(
            queue
                .decide(&id, ApprovalDecision::Grant { scope }, "android", None)?
                .grant_scope,
            Some(scope)
        );
        assert!(!queue.consume_once(&id, &owner, &action)?);
    }
    let (_, mut updates) = queue.pending()?;
    queue.expire_at(|| deadline - 1)?;
    assert!(matches!(
        updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    queue.expire_at(|| deadline)?;
    for _ in 0..2 {
        assert_eq!(updates.try_recv()?.state, ApprovalState::Expired);
    }
    Ok(())
}
