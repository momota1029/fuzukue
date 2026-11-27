use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde::Serialize;

use super::{delivery_now, Journal, Record, Work, INTERRUPTED};
use crate::delegate::DeskTurn;

#[derive(Serialize)]
pub(crate) struct DeskQueue {
    items: Vec<QueueItem>,
    total: usize,
}

#[derive(Serialize)]
struct QueueItem {
    id: String,
    kind: &'static str,
    claimed: bool,
    review_reason: Option<&'static str>,
    session: String,
    age_secs: Option<i64>,
    mentions_job_ids: Vec<String>,
}

impl Journal {
    pub(crate) fn desk_queue(
        &self,
        dir: &Path,
        job_ids: &[String],
        offset: usize,
        limit: usize,
    ) -> Result<DeskQueue> {
        let mut state = self.0.lock().unwrap();
        Self::load(dir, &mut state)?;
        let now = delivery_now();
        let matching = state
            .records
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|record| Some((record, desk_kind(&record.work)?)));
        let total = matching.clone().count();
        let items = matching
            .skip(offset)
            .take(limit)
            .map(|(record, kind)| QueueItem {
                id: record.id.clone(),
                kind,
                claimed: record.claimed,
                review_reason: record.review.as_deref().map(review_reason),
                session: record.session.clone(),
                age_secs: record
                    .created_at
                    .map(|at| now.saturating_sub(at).max(0) / 1000),
                mentions_job_ids: job_ids
                    .iter()
                    .filter(|id| mentions_job(record, id))
                    .cloned()
                    .collect(),
            })
            .collect();
        Ok(DeskQueue { items, total })
    }

    pub(crate) fn redeliver(&self, dir: &Path, session: &str, id: &str) -> Result<()> {
        self.edit(dir, session, |records| {
            let record = records
                .iter_mut()
                .find(|record| record.id == id)
                .context("Held work item not found")?;
            ensure!(
                matches!(
                    &record.work,
                    Work::Desk(DeskTurn::Report { .. } | DeskTurn::WorkshopReport { approval_id: Some(_), .. })
                        | Work::Claude(_) | Work::Delegate(_)
                ),
                "This held work kind does not support redelivery"
            );
            // A review record has left normal submission. An unclaimed record can
            // also be made eligible earlier without discarding its retry history.
            if record.review.take().is_some() {
                record.claimed = false;
                record.preparing = false;
                record.retry = None;
            } else {
                let now = delivery_now();
                let retry = record
                    .retry
                    .as_mut()
                    .filter(|retry| !record.claimed && retry.next_attempt_at > now)
                    .context("Held work item is already pending or in progress")?;
                retry.next_attempt_at = now;
            }
            Ok(())
        })?
    }
}

fn desk_kind(work: &Work) -> Option<&'static str> {
    match work {
        Work::Desk(DeskTurn::Report { .. }) => Some("Desk Report"),
        Work::Desk(DeskTurn::WorkshopReport { approval_id: Some(_), .. }) => Some("Approval Result"),
        Work::Claude(_) => Some("Claude"),
        Work::Delegate(_) => Some("Delegate"),
        Work::Checkpoint { .. } => Some("Checkpoint"),
        Work::Usage(_) => Some("Usage"),
        _ => None,
    }
}

fn mentions_job(record: &Record, job_id: &str) -> bool {
    match &record.work {
        Work::Desk(DeskTurn::Report { job, .. }) => job == job_id,
        Work::Delegate(_) => record.id == job_id,
        Work::Checkpoint { result, .. } => {
            result
                .lines()
                .next()
                .and_then(|line| {
                    line.strip_prefix("Finished. Job ")
                        .or_else(|| line.strip_prefix("Review required before resuming. Job "))
                })
                .and_then(|job| job.strip_suffix('.'))
                == Some(job_id)
        }
        _ => false,
    }
}

fn review_reason(reason: &str) -> &'static str {
    // Persisted reasons can be backend errors or transcript text. Expose only
    // exact matches to messages whose contents are fixed in the server.
    match reason {
        INTERRUPTED => INTERRUPTED,
        "相手を替えるには Claude のターンの完了を待つ" => {
            "Delivery could not switch backends while Claude was running."
        }
        "The target thread is closed" => "The target thread is closed.",
        "Claude exited during this turn. Review the saved transcript." => {
            "Claude exited during this turn. Review the saved transcript."
        }
        "The turn was interrupted. Review the saved transcript before resuming." => {
            "The turn was interrupted. Review the saved transcript before resuming."
        }
        _ => "Delivery requires review. Stored details are withheld.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redelivery_advances_backoff_without_discarding_retry_history() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let journal = Journal::default();
        let session = "thread-fixture";
        let id = journal.enqueue(
            dir.path(),
            session,
            "held-report".into(),
            Work::Desk(DeskTurn::Report {
                job: "delegate:test-job".into(),
                text: "report body".into(),
                plan_failed: false,
            }),
        )?;
        let refused_at = delivery_now();
        journal.edit(dir.path(), session, |records| {
            let record = &mut records[0];
            record.created_at = None;
            record.requeue("backend busy".into(), refused_at);
            record.retry.as_mut().unwrap().next_attempt_at = refused_at + 60_000;
        })?;
        let queue = journal.desk_queue(dir.path(), &[], 0, 1)?;
        assert!(queue.items[0].age_secs.is_none());
        assert!(!journal.records(dir.path())?[0].ready(delivery_now()));
        journal.redeliver(dir.path(), session, &id)?;
        assert!(journal.redeliver(dir.path(), session, &id).is_err());

        let reopened = Journal::default();
        let records = reopened.records(dir.path())?;
        assert!(records[0].ready(delivery_now()));
        let retry = records[0].retry.as_ref().unwrap();
        assert_eq!(retry.first_refused_at, refused_at);
        assert_eq!(retry.attempts, 1);
        assert!(reopened.claim(dir.path(), session, &id)?);
        assert!(!reopened.claim(dir.path(), session, &id)?);
        Ok(())
    }
}
