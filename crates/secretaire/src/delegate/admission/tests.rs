use std::path::Path;

use super::*;

fn writes(globs: Option<&[&str]>) -> Arc<WriteSet> {
    Arc::new(
        WriteSet::new(
            Path::new("/fixture"),
            globs.map(|globs| globs.iter().map(|glob| (*glob).into()).collect()),
            "",
            || None,
        )
        .unwrap(),
    )
}

fn entry(id: &str, globs: Option<&[&str]>, phase: Phase) -> Entry {
    Entry {
        id: id.into(),
        label: id.into(),
        writes: writes(globs),
        phase,
        lane: None,
    }
}

#[test]
fn a_tight_backend_lowers_its_own_parallelism_only() {
    use fuzukue_proto::AgentBackend::{Claude, Codex};

    let mut queue = Queue::default();
    let mut active = entry("active", Some(&[]), Phase::Running);
    active.lane = Some(Lane { backend: Codex, limit: None });
    queue.jobs.push_back(active);
    let mut next = entry("next", Some(&[]), Phase::Ready);
    next.lane = Some(Lane { backend: Codex, limit: Some(1) });
    assert!(matches!(
        queue.can_run(&next, Config::default()),
        Err(WaitReason::UsageParallel { running: 1, limit: 1, .. })
    ));
    next.lane = Some(Lane { backend: Claude, limit: Some(1) });
    assert!(queue.can_run(&next, Config::default()).is_ok());
}

#[test]
fn usage_settings_come_from_the_delegate_section() -> Result<()> {
    let root = tempfile::tempdir()?;
    assert!(Config::load(root.path(), Default::default())?.usage.reroute);
    std::fs::write(
        root.path().join("fuzukue.toml"),
        "[delegate]\nusage_reroute = false\nusage_pacing_from = 0.5\nusage_pacing_running = 2\n",
    )?;
    let usage = Config::load(root.path(), Default::default())?.usage;
    assert_eq!((usage.reroute, usage.pacing, usage.pacing_from, usage.tight_running), (false, true, 0.5, 2));
    std::fs::write(root.path().join("fuzukue.toml"), "[delegate]\nusage_pacing_from = 2.0\n")?;
    assert!(Config::load(root.path(), Default::default()).is_err());
    Ok(())
}

#[test]
fn soft_limit_only_promotes_known_disjoint_jobs_and_hard_limit_always_applies() {
    let config = Config {
        max_parallel: 1,
        max_parallel_hard: 2,
        ..Config::default()
    };
    let mut queue = Queue::default();
    let matching = entry("matching", Some(&["crates/**/src/lib.rs"]), Phase::Ready);
    let disjoint = entry("disjoint", Some(&["crates/a/src/*.toml"]), Phase::Ready);
    let unknown = entry("unknown", None, Phase::Ready);
    let read_only = entry("read-only", Some(&[]), Phase::Ready);
    assert!(queue.can_run(&unknown, config).is_ok());

    queue.jobs.push_back(entry(
        "active",
        Some(&["crates/a/src/*.rs"]),
        Phase::Running,
    ));
    assert!(matches!(
        queue.can_run(&matching, config),
        Err(WaitReason::Overlap { job, path: Some(path), .. })
            if job == "active" && path == "crates/a/src/*.rs"
    ));
    assert!(queue.can_run(&disjoint, config).is_ok());
    assert!(matches!(
        queue.can_run(&unknown, config),
        Err(WaitReason::SoftCap {
            running: 1,
            limit: 1,
            unknown_write_set: true
        })
    ));

    queue.jobs[0] = entry("unknown-active", None, Phase::Running);
    assert!(matches!(
        queue.can_run(&disjoint, config),
        Err(WaitReason::Overlap { job, path: None, .. }) if job == "unknown-active"
    ));
    assert!(queue.can_run(&read_only, config).is_ok());
    queue
        .jobs
        .push_back(entry("at-ceiling", Some(&[]), Phase::Running));
    assert!(matches!(
        queue.can_run(&read_only, config),
        Err(WaitReason::HardCap {
            running: 2,
            limit: 2
        })
    ));
}

fn with_capacity(soft: usize, hard: usize) -> Arc<Scheduler> {
    Arc::new(Scheduler::new(Config {
        max_parallel: soft,
        max_parallel_hard: hard,
        max_load_per_cpu: 0.0,
        min_available_memory_mib: 0,
        ..Config::default()
    }))
}

fn register(scheduler: &Arc<Scheduler>, name: &str, paths: &[&str]) -> Request {
    scheduler.register(name.into(), name.into(), writes(Some(paths)))
}

#[tokio::test]
async fn queue_runs_its_first_admissible_job_and_reports_the_conflict() {
    let scheduler = with_capacity(1, 2);
    let active = register(&scheduler, "active", &["src/**"]);
    let active_slot = active.acquire(|_| {}).await;
    let blocked = register(&scheduler, "blocked", &["src/main.rs"]);
    let unrelated = register(&scheduler, "unrelated", &["docs/**"]);
    let reported = std::cell::RefCell::new(Vec::new());
    let mut first = std::pin::pin!(blocked.acquire(|reason| reported.borrow_mut().push(reason)));
    assert!(futures_util::poll!(&mut first).is_pending());
    let other = unrelated.acquire(|_| {}).await;
    assert!(futures_util::poll!(&mut first).is_pending());

    drop(active_slot);
    let slot = first.await;
    drop(slot);
    drop(other);
    assert_eq!(
        reported
            .borrow()
            .first()
            .map(WaitReason::message)
            .as_deref(),
        Some(harness_text!(
            "waiting for active (overlapping src/**)",
            "active の終了待ち（変更予定が重複: src/**）"
        ))
    );
}

#[tokio::test]
async fn hold_all_slots_waits_for_every_configured_slot_including_burst_slots() {
    let scheduler = with_capacity(2, 3);
    let a = register(&scheduler, "a", &["a"]);
    let b = register(&scheduler, "b", &["b"]);
    let c = register(&scheduler, "c", &["c"]);
    let a_slot = a.acquire(|_| {}).await;
    let b_slot = b.acquire(|_| {}).await;
    let c_slot = c.acquire(|_| {}).await;
    let mut hold = std::pin::pin!(scheduler.hold_all_slots());
    assert!(futures_util::poll!(&mut hold).is_pending());
    drop(a_slot);
    drop(b_slot);
    assert!(futures_util::poll!(&mut hold).is_pending());

    drop(c_slot);
    let held = hold.await;
    let mut acquire = std::pin::pin!(a.acquire(|_| {}));
    assert!(futures_util::poll!(&mut acquire).is_pending());
    drop(held);
    drop(acquire.await);
}

#[test]
fn load_guard_uses_strict_thresholds_and_skips_missing_samples() {
    let config = Config::default();
    let minimum = 4096 * 1024;
    let sample = |load, available_kib| Headroom {
        load,
        available_kib,
    };
    assert!(sample(Some((5.99, 4)), Some(minimum + 1))
        .blocked(config)
        .is_none());
    assert!(matches!(
        sample(Some((6.0, 4)), None).blocked(config),
        Some(LoadWait::Cpu { .. })
    ));
    assert!(matches!(
        sample(None, Some(minimum)).blocked(config),
        Some(LoadWait::Memory { .. })
    ));
    assert!(sample(None, None).blocked(config).is_none());

    let disabled = Config {
        max_load_per_cpu: 0.0,
        min_available_memory_mib: 0,
        ..config
    };
    assert!(sample(Some((100.0, 4)), Some(0))
        .blocked(disabled)
        .is_none());
}

#[test]
fn workspace_delegate_settings_override_server_fields() -> Result<()> {
    let root = tempfile::tempdir()?;
    let server = Overrides {
        max_parallel: Some(2),
        max_parallel_hard: Some(9),
        max_load_per_cpu: Some(2.5),
        ..Overrides::default()
    };
    std::fs::write(
        root.path().join("fuzukue.toml"),
        "[delegate]\nmax_parallel = 5\nmin_available_memory_mib = 1024\n",
    )?;
    let config = Config::load(root.path(), server)?;
    assert_eq!((config.max_parallel, config.max_parallel_hard), (5, 9));
    assert_eq!(config.max_load_per_cpu, 2.5);
    assert_eq!(config.min_available_memory_mib, 1024);
    Ok(())
}
