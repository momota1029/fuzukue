use std::path::Path;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    Actor, FolderGrant, FolderGrantExpiry, FolderGrantRead, LocalizedMessage, ServerEvent,
    FOLDER_ACCESS_REQUEST,
};

use super::{ApprovalWaiter, Hub, Inner, PendingApproval};
use crate::folder_grants::{canonical_folder, Store};

impl Inner {
    fn folder_store(&self) -> Result<&Store> {
        self.folder_grants
            .as_ref()
            .context("Folder grants are unavailable")
    }

    fn folder_store_mut(&mut self) -> Result<&mut Store> {
        self.folder_grants
            .as_mut()
            .context("Folder grants are unavailable")
    }

    fn current_turn(&self, session: &str) -> Option<Instant> {
        self.partner_started
            .get(session)
            .map(|started| started.instant)
    }

    fn live_thread(&self, session: &str) -> bool {
        self.threads
            .as_ref()
            .is_some_and(|threads| threads.is_live(session))
    }
}

fn request(approval: &PendingApproval) -> Option<(&str, &str)> {
    let ServerEvent::ApprovalRequested {
        session,
        request_id,
        localized: Some(message),
        ..
    } = &approval.envelope.event
    else {
        return None;
    };
    (request_id.starts_with(FOLDER_ACCESS_REQUEST)
        && matches!(approval.waiter, ApprovalWaiter::Folder))
    .then_some((session.as_str(), message.args.get("path")?.as_str()))
}

impl Hub {
    pub(crate) fn request_folder_access(&self, session: &str, path: &Path) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let folder = canonical_folder(path, &inner.folder_store()?.protected())?;
        let mut pending = 0;
        for (id, approval) in &inner.approvals {
            if let Some((_, path)) = request(approval).filter(|(owner, _)| *owner == session) {
                if path == folder {
                    return Ok(id.clone());
                }
                pending += 1;
            }
        }
        ensure!(pending < 20, "Too many pending folder access requests");
        let request_id = format!("{FOLDER_ACCESS_REQUEST}{}", crate::mail::session_key());
        self.publish_locked(
            &mut inner,
            ServerEvent::ApprovalRequested {
                session: session.into(),
                request_id: request_id.clone(),
                actor: Actor::System,
                summary: format!("Read-only folder access: {folder}"),
                localized: Some(LocalizedMessage::new(
                    "server-folder-access-request",
                    [("path", folder)],
                )),
                shell_directory: None,
                shell_prefix: None,
            },
            None,
            ApprovalWaiter::Folder,
        );
        Ok(request_id)
    }

    pub(crate) fn decide_folder_access(
        &self,
        request_id: &str,
        expiry: Option<FolderGrantExpiry>,
        principal: &str,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let (session, path) = inner
            .approvals
            .get(request_id)
            .and_then(request)
            .context("This folder access request is no longer pending")?;
        let (session, path) = (session.to_owned(), path.to_owned());
        if let Some(expiry) = expiry {
            // Approval must still refer to the directory shown when the request was made.
            let current = canonical_folder(Path::new(&path), &inner.folder_store()?.protected())?;
            ensure!(
                current == path,
                "The requested folder changed before approval"
            );
            self.add_folder_grant_locked(&mut inner, &session, path, expiry, principal)?;
        }
        self.publish_locked(
            &mut inner,
            ServerEvent::ApprovalResolved {
                request_id: request_id.into(),
                decision: if expiry.is_some() {
                    "accept"
                } else {
                    "decline"
                }
                .into(),
                automatic: false,
            },
            None,
            ApprovalWaiter::Backend,
        );
        Ok(())
    }

    pub(crate) fn add_folder_grant(
        &self,
        session: &str,
        path: &Path,
        expiry: FolderGrantExpiry,
        principal: &str,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let folder = canonical_folder(path, &inner.folder_store()?.protected())?;
        ensure!(
            Path::new(&folder) == path,
            "Use the canonical folder path without symlinks"
        );
        self.add_folder_grant_locked(&mut inner, session, folder, expiry, principal)
    }

    fn add_folder_grant_locked(
        &self,
        inner: &mut Inner,
        session: &str,
        path: String,
        expiry: FolderGrantExpiry,
        principal: &str,
    ) -> Result<()> {
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let turn = inner.current_turn(session);
        inner
            .folder_store_mut()?
            .add(session.into(), path, expiry, turn, principal)?;
        let _ = self
            .tx
            .send(crate::snapshot(ServerEvent::FolderGrantsChanged {
                session: session.into(),
            }));
        Ok(())
    }

    pub(crate) fn list_folder_grants(
        &self,
        session: &str,
        before: Option<u64>,
    ) -> Result<(Vec<FolderGrant>, Vec<FolderGrantRead>, Option<u64>)> {
        let inner = self.inner.lock().unwrap();
        ensure!(
            inner
                .threads
                .as_ref()
                .is_some_and(|threads| threads.contains(session)),
            "Thread not found"
        );
        let store = inner.folder_store()?;
        let grants = if inner.live_thread(session) {
            store
                .active(
                    session,
                    chrono::Utc::now().timestamp_millis(),
                    inner.current_turn(session),
                )
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let (audit, before) = store.audit(session, before)?;
        Ok((grants, audit, before))
    }

    pub(crate) fn revoke_folder_grant(&self, session: &str, id: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .folder_store_mut()?
            .revoke(session, id)?;
        let _ = self
            .tx
            .send(crate::snapshot(ServerEvent::FolderGrantsChanged {
                session: session.into(),
            }));
        Ok(())
    }

    pub(crate) fn validate_worker_grants(&self, session: &str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let inner = self.inner.lock().unwrap();
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let active = inner
            .folder_store()?
            .active(
                session,
                chrono::Utc::now().timestamp_millis(),
                inner.current_turn(session),
            )
            .map(|grant| grant.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        ensure!(
            ids.len() <= 100 && ids.iter().all(|id| active.contains(id.as_str())),
            "Delegation listed an inactive or unknown folder grant"
        );
        Ok(())
    }

    pub(crate) fn folder_read(
        &self,
        session: &str,
        path: &Path,
        ids: Option<&[String]>,
        audit: bool,
    ) -> Result<bool> {
        let inner = self.inner.lock().unwrap();
        if !inner.live_thread(session) {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let store = inner.folder_store()?;
        let Some(grant) = store
            .active(session, now, inner.current_turn(session))
            .filter(|grant| ids.is_none_or(|ids| ids.contains(&grant.id)))
            .filter(|grant| path.starts_with(&grant.path))
            .max_by_key(|grant| grant.path.len())
        else {
            return Ok(false);
        };
        if audit {
            store
                .record(session, grant, path, now)
                .context("Could not record folder read")?;
        }
        Ok(true)
    }

    pub(super) fn end_folder_grants(
        &self,
        inner: &mut Inner,
        session: &str,
        trash: bool,
    ) -> Result<()> {
        inner.folder_store_mut()?.end_thread(session, trash)?;
        self.close_approvals(inner, "cancelled", |approval| {
            request(approval).is_some_and(|(owner, _)| owner == session)
        });
        let _ = self
            .tx
            .send(crate::snapshot(ServerEvent::FolderGrantsChanged {
                session: session.into(),
            }));
        Ok(())
    }

    pub(crate) fn set_thread_archived(&self, session: &str, archived: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if archived {
            self.end_folder_grants(&mut inner, session, false)?;
        }
        let store = inner
            .threads
            .as_mut()
            .expect("Workspace initializes the thread store");
        store.set_archived(session, archived)?;
        let _ = self.tx.send(crate::snapshot(store.snapshot()));
        Ok(())
    }
}
