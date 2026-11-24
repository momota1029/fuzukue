//! Standalone changes that must reach a recorded outcome before the server swaps.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use fuzukue_proto::{Actor, LocalizedMessage, ServerEvent};
use tokio::sync::{Notify, OwnedRwLockReadGuard};

use crate::{
    cancellation::Cancellation,
    delegate,
    self_update::{drain::Work, Update},
    App, Workspace,
};

const LIMIT: Duration = Duration::from_secs(180);
const PREFIX: &str = "mutation:";

fn update_busy() -> &'static str {
    harness_text!(
        "A server update or shutdown is in progress. Try again once it finishes.",
        "サーバーの更新または停止のため、操作を受け付けられない。稼働に戻ってからやり直してね。",
    )
}

#[derive(Default)]
pub(crate) struct Mutations {
    active: Mutex<HashMap<String, Arc<Cancellation>>>,
    finished: Notify,
}

impl Mutations {
    pub(crate) fn count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    pub(crate) async fn interrupt(&self) {
        for cancel in self.active.lock().unwrap().values() {
            cancel.cancel();
        }
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.lock().unwrap().is_empty() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct Mutation<'a> {
    app: &'a Workspace,
    registry: Option<Arc<App>>,
    admission: Option<(Arc<Update>, i64)>,
    _permit: Option<OwnedRwLockReadGuard<()>>,
    cancel: Arc<Cancellation>,
    session: &'a str,
    id: String,
    label: String,
    running: AtomicBool,
    writing: AtomicBool,
}

impl<'a> Mutation<'a> {
    pub(crate) fn new(app: &'a Workspace, session: &'a str, label: String) -> Result<Self> {
        let registry = app.registry.upgrade();
        let id = format!("{PREFIX}{}", crate::mail::session_key());
        let cancel = Arc::new(Cancellation::default());
        let (admission, permit) = if let Some(registry) = &registry {
            // Serialize registration with interruption. A drain either sees this request
            // in the registry or closes admission before it can register.
            let mut active = registry.mutations.active.lock().unwrap();
            let admitted = registry
                .self_update
                .get()
                .map(|update| {
                    let (permit, epoch) = update.admit_mutation(None).context(update_busy())?;
                    Ok::<_, anyhow::Error>(((update.clone(), epoch), permit))
                })
                .transpose()?;
            active.insert(id.clone(), cancel.clone());
            admitted.unzip()
        } else {
            (None, None)
        };
        Ok(Self {
            app,
            registry,
            admission,
            _permit: permit,
            cancel,
            session,
            id,
            label,
            running: AtomicBool::new(false),
            writing: AtomicBool::new(false),
        })
    }

    pub(crate) async fn prepare<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(anyhow!(update_busy())),
            result = tokio::time::timeout(LIMIT, future) => result.context("Timed out preparing the operation")?,
        }
    }

    pub(crate) async fn approve(
        &self,
        prefix: &str,
        actor: Actor,
        summary: LocalizedMessage,
    ) -> Result<()> {
        let admission = self.admission.clone();
        let cancel = self.cancel.clone();
        // Human response time does not count against the execution deadline.
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(anyhow!(update_busy())),
            decision = delegate::request_mutation_approval(self.app, self.session, prefix, actor, summary, move || {
                cancel.check().is_ok() && admission.is_none_or(|(update, epoch)| {
                    // The original permit remains held while the answer is delivered and used.
                    update.admit_mutation(Some(epoch)).is_ok()
                })
            }) => decision?,
        };
        ensure!(
            decision != delegate::MUTATION_APPROVAL_EXPIRED,
            "{}",
            update_busy()
        );
        ensure!(decision == "accept", "利用者が承認しなかった");
        Ok(())
    }

    pub(crate) fn begin_write(&self) {
        self.writing.store(true, Ordering::Relaxed);
    }

    pub(crate) async fn run<T>(&self, action: impl Future<Output = Result<T>>) -> Result<T> {
        let journal = &self.app.drain;
        let dir = &self.app.state_dir;
        journal.enqueue(
            dir,
            self.session,
            self.id.clone(),
            Work::Checkpoint {
                result: self.label.clone(),
                directory: self.app.project_dir.clone(),
            },
        )?;
        ensure!(
            journal.claim(dir, self.session, &self.id)?,
            "Could not claim operation"
        );
        self.running.store(true, Ordering::Relaxed);
        let result = {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => None,
                result = tokio::time::timeout(LIMIT, action) => Some(result),
            }
        };
        let result = match result {
            None => Err(self.needs_review("The server interrupted the operation.")),
            Some(Err(_)) => Err(self.needs_review("The operation timed out.")),
            Some(Ok(Err(error))) if self.writing.load(Ordering::Relaxed) => Err(error.context(
                self.needs_review("The operation did not confirm completion.")
                    .to_string(),
            )),
            Some(Ok(result)) => result,
        };
        if self.running.swap(false, Ordering::Relaxed) {
            journal.finish(dir, self.session, &self.id, None);
            if self.app.hub.has_thread(self.session) {
                let outcome = if result.is_ok() {
                    "completed"
                } else {
                    "failed before applying changes"
                };
                self.app.hub.publish(ServerEvent::LocalNotice {
                    session: self.session.into(),
                    item: self.id.clone(),
                    text: format!("{} — {outcome}", self.label),
                    localized: None,
                });
            }
        }
        result
    }

    fn needs_review(&self, reason: &str) -> anyhow::Error {
        let message = format!(
            "{} — needs review. {reason} Check the local and remote state before retrying.",
            self.label
        );
        self.app.drain.finish(
            &self.app.state_dir,
            self.session,
            &self.id,
            Some(message.clone()),
        );
        self.running.store(false, Ordering::Relaxed);
        publish_review(self.app, self.session, &self.id, message.clone());
        anyhow!(message)
    }
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        if *self.running.get_mut() {
            let _ = self.needs_review("The operation stopped before confirming completion.");
        }
        if let Some(registry) = &self.registry {
            registry.mutations.active.lock().unwrap().remove(&self.id);
            registry.mutations.finished.notify_waiters();
        }
    }
}

pub(crate) fn recover(app: &Workspace, record: &crate::self_update::drain::Record) {
    if record.id.starts_with(PREFIX) && record.claimed && record.review.is_none() {
        if let Work::Checkpoint { result, .. } = &record.work {
            let message = format!("{result} — needs review. The previous server stopped before confirming completion. Check the local and remote state before retrying.");
            app.drain.finish(
                &app.state_dir,
                &record.session,
                &record.id,
                Some(message.clone()),
            );
            publish_review(app, &record.session, &record.id, message);
        }
    }
}

fn publish_review(app: &Workspace, session: &str, id: &str, message: String) {
    if app.hub.has_thread(session) {
        app.hub.publish(ServerEvent::LocalNotice {
            session: session.into(),
            item: id.into(),
            text: message,
            localized: None,
        });
    } else {
        app.hub.publish(ServerEvent::Error { message });
    }
}
