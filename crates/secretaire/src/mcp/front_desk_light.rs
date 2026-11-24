use anyhow::{ensure, Result};
use fuzukue_proto::{SelfUpdateStatus, SelfUpdateStepState};
use serde::Deserialize;
use serde_json::{json, Value};

use super::parse_args;
use super::workspace_read::range::{IntegerRange, optional_integer};
use crate::Workspace;

const DEFAULT_LIMIT: usize = 50;
const DEFAULT_OFFSET: usize = 0;
const QUEUE_LIMIT: IntegerRange = IntegerRange::new(1, 100);
const QUEUE_OFFSET: IntegerRange = IntegerRange::new(0, u32::MAX as usize);
const MAX_JOB_IDS: usize = 32;
const MAX_LOG_LINES: usize = 40;
const MAX_LOG_BYTES: usize = 2048;

pub(super) fn queue_schema() -> (&'static str, Value, &'static [&'static str]) {
    (
        harness_text!(
            "List held-work metadata and links to the given full job IDs.",
            "保留中の仕事の状態と、指定した完全なジョブ ID との関連を一覧する。"
        ),
        json!({
            "job_ids": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 128}, "maxItems": MAX_JOB_IDS},
            "offset": QUEUE_OFFSET.property(DEFAULT_OFFSET),
            "limit": QUEUE_LIMIT.property(DEFAULT_LIMIT),
        }),
        &[],
    )
}

pub(super) fn redeliver_schema() -> (&'static str, Value, &'static [&'static str]) {
    (
        harness_text!(
            "Return a held item to pending. Check its prior delivery and set confirm=true after user approval.",
            "保留された仕事を待機状態に戻す。以前の配信を確認し、利用者の了承を得てから confirm=true を指定する。"
        ),
        json!({"id": {"type": "string", "minLength": 1}, "confirm": {"type": "boolean"}}),
        &["id", "confirm"],
    )
}

pub(super) fn queue(app: &Workspace, args: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[serde(default)]
        job_ids: Vec<String>,
        #[serde(default, deserialize_with = "optional_integer")]
        offset: Option<i128>,
        #[serde(default, deserialize_with = "optional_integer")]
        limit: Option<i128>,
    }
    let Args {
        job_ids,
        offset,
        limit,
    } = parse_args(args)?;
    let limit = QUEUE_LIMIT.resolve(limit, DEFAULT_LIMIT);
    let offset = QUEUE_OFFSET.resolve(offset, DEFAULT_OFFSET);
    ensure!(
        job_ids.len() <= MAX_JOB_IDS && job_ids.iter().all(|id| !id.is_empty() && id.len() <= 128),
        "Supply at most {MAX_JOB_IDS} full job IDs"
    );
    let queue = app
        .drain
        .desk_queue(&app.state_dir, &job_ids, offset, limit)
        .map_err(|_| anyhow::anyhow!("Could not read the held work queue"))?;
    Ok(serde_json::to_value(queue)?)
}

pub(super) fn redeliver(app: &Workspace, session: &str, args: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        id: String,
        #[serde(default)]
        confirm: bool,
    }
    let Args { id, confirm } = parse_args(args)?;
    ensure!(
        confirm,
        harness_text!(
            "Ask the user and set confirm=true to redeliver this item.",
            "利用者の了承を得てから confirm=true を指定する。"
        )
    );
    ensure!(!id.trim().is_empty(), "Specify the item ID from desk_queue");
    let _permit = app.admit_work()?;
    app.drain.redeliver(&app.state_dir, session, &id)?;
    Ok(json!({"id": id, "pending": true}))
}

pub(super) fn update_status(status: SelfUpdateStatus, verbose: bool) -> Result<Value> {
    let stage = [
        SelfUpdateStepState::Running,
        SelfUpdateStepState::Failed,
        SelfUpdateStepState::Passed,
    ]
    .into_iter()
    .find_map(|state| status.steps.iter().rev().find(|step| step.state == state))
    .map(|step| step.kind);
    let steps: Vec<_> = status
        .steps
        .iter()
        .map(|step| {
            let mut value = json!({"kind": step.kind, "state": step.state});
            if verbose {
                add_log(&mut value, &step.log);
            }
            value
        })
        .collect();
    let mut value = if verbose {
        serde_json::to_value(&status)?
    } else {
        json!({})
    };
    if let Some(canary) = status.canary_status.as_ref().filter(|_| verbose) {
        add_log(&mut value["canary_status"], &canary.log);
    }
    value["active"] = json!(status.active);
    value["stage"] = json!(stage);
    value["wait_reason"] = json!(status.wait.as_ref().map(|wait| &wait.reason));
    value["running_build_id"] = json!(status.build_id);
    value["source_commit"] = json!(status.source_commit);
    value["error"] = json!(status.error);
    value["steps"] = json!(steps);
    Ok(value)
}

fn add_log(value: &mut Value, log: &str) {
    let line_start = log
        .rmatch_indices('\n')
        .nth(MAX_LOG_LINES - usize::from(!log.ends_with('\n')))
        .map_or(0, |(index, _)| index + 1);
    let mut start = line_start.max(log.len().saturating_sub(MAX_LOG_BYTES));
    while !log.is_char_boundary(start) {
        start += 1;
    }
    value["log"] = json!(&log[start..]);
    if start > 0 {
        value["log_truncated"] = true.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::{
        SelfUpdateCanaryState, SelfUpdateCanaryStatus, SelfUpdateStep, SelfUpdateStepKind,
        SelfUpdateWait, SelfUpdateWaitReason,
    };

    #[test]
    fn desk_queue_clamps_page_limit_and_offset() -> Result<()> {
        use crate::mcp::workspace_read::range::tests::{boundary_cases, wrong_types};
        use crate::self_update::drain::Work;

        let project = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let app = Workspace::new(
            project.path().into(),
            "127.0.0.1:7878".into(),
            state.path().into(),
            std::sync::Arc::new(crate::Hub::new()),
        );
        for i in 0..101 {
            app.drain.enqueue(
                &app.state_dir,
                "thread",
                format!("item-{i}"),
                Work::Checkpoint {
                    result: "completed".into(),
                    directory: ".".into(),
                },
            )?;
        }
        for (limit, expected) in boundary_cases(1, 100, 50) {
            let args = limit.map_or_else(|| json!({}), |limit| json!({"limit": limit}));
            let result = queue(&app, &args)?;
            assert_eq!(result["items"].as_array().unwrap().len(), expected);
        }
        for (offset, expected) in boundary_cases(0, u32::MAX as usize, 0) {
            let mut args = json!({"limit": 1});
            if let Some(offset) = offset {
                args["offset"] = offset;
            }
            let result = queue(&app, &args)?;
            assert_eq!(
                result["items"].as_array().unwrap().len(),
                usize::from(expected == 0)
            );
        }
        for field in ["offset", "limit"] {
            for value in wrong_types() {
                let mut args = json!({});
                args[field] = value;
                let error = queue(&app, &args).unwrap_err();
                assert!(format!("{error:#}").contains("expected an integer"), "{field}");
            }
        }
        Ok(())
    }

    #[test]
    fn compact_update_status_contains_step_state_and_wait_reason() -> Result<()> {
        let status = SelfUpdateStatus {
            build_id: "running".into(),
            source_commit: Some("candidate".into()),
            active: true,
            wait: Some(SelfUpdateWait {
                started_at: 42,
                elapsed_secs: 7,
                reason: SelfUpdateWaitReason {
                    gate_held: true,
                    ..Default::default()
                },
                forcing: false,
            }),
            steps: vec![
                SelfUpdateStep {
                    kind: SelfUpdateStepKind::Build,
                    state: SelfUpdateStepState::Passed,
                    log: "private build log".into(),
                },
                SelfUpdateStep {
                    kind: SelfUpdateStepKind::Wait,
                    state: SelfUpdateStepState::Running,
                    log: "private wait log".into(),
                },
                SelfUpdateStep {
                    kind: SelfUpdateStepKind::Swap,
                    state: SelfUpdateStepState::Pending,
                    log: String::new(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            update_status(status, false)?,
            json!({
                "active": true, "stage": "wait", "running_build_id": "running", "source_commit": "candidate",
                "error": null, "wait_reason": {"threads": [], "gate_held": true, "checking_sessions": false},
                "steps": [{"kind": "build", "state": "passed"}, {"kind": "wait", "state": "running"}, {"kind": "swap", "state": "pending"}]
            })
        );
        Ok(())
    }

    #[test]
    fn verbose_update_status_limits_each_log_and_retains_trial_url() -> Result<()> {
        let history: String = (0..60).map(|line| format!("line {line}\n")).collect();
        let trial = "長いログ".repeat(10_000);
        let status = SelfUpdateStatus {
            steps: vec![SelfUpdateStep {
                kind: SelfUpdateStepKind::Build,
                state: SelfUpdateStepState::Failed,
                log: history.clone(),
            }],
            canary_status: Some(SelfUpdateCanaryStatus {
                state: SelfUpdateCanaryState::Running,
                commit: None,
                port: Some(7879),
                connect_url: Some("ws://127.0.0.1:7879/ws".into()),
                started_at: None,
                auto_discard_at: None,
                log: trial,
            }),
            ..Default::default()
        };
        let verbose = update_status(status, true)?;
        assert_eq!(verbose["stage"], "build");
        let step = &verbose["steps"][0];
        let log = step["log"].as_str().unwrap();
        assert_eq!(log.lines().count(), MAX_LOG_LINES);
        assert!(log.starts_with("line 20\n") && log.ends_with("line 59\n"));
        assert_eq!(step["log_truncated"], true);
        assert_eq!(
            verbose["canary_status"]["connect_url"],
            "ws://127.0.0.1:7879/ws"
        );
        assert!(verbose["canary_status"]["log"].as_str().unwrap().len() <= MAX_LOG_BYTES);
        assert_eq!(verbose["canary_status"]["log_truncated"], true);
        let mut step = json!({});
        add_log(&mut step, history.trim_end());
        assert_eq!(step["log"].as_str().unwrap(), log.trim_end());
        Ok(())
    }
}
