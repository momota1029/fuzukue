use std::path::Path;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{
    Actor, FolderGrant, FolderGrantExpiry, FolderGrantRead, LocalizedMessage, ServerEvent,
    FOLDER_ACCESS_REQUEST, ApprovalRisk,
};

use super::{ApprovalWaiter, Hub, Inner, PendingApproval, approval_store::Registration};
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
            .map(|started| started.observed.instant)
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

pub(super) fn full_registration(session: &str, job: Option<&str>, folder: &str, request: &str) -> Registration {
    let mut owner = crate::full_approval::desk_owner(session);
    if let Some(job) = job {
        owner.job = job.into();
        owner.requester = "worker".into();
    }
    crate::full_approval::registration(
        owner, "folder_read", ApprovalRisk::Mid,
        serde_json::json!({"expiry": "one_hour", "request": request}),
        serde_json::json!({"folder": folder}),
    )
}

pub(crate) enum AccessRequest {
    Granted(String),
    Pending(String),
}

impl Hub {
    pub(super) fn plan_folder_sweep(&self, inner: &Inner) -> (Vec<FolderGrant>, Vec<String>) {
        let mut grants = Vec::new();
        let mut invalid = Vec::new();
        let Ok(store) = inner.folder_store() else { return (grants, invalid) };
        let protected = store.protected();
        let now = chrono::Utc::now().timestamp_millis();
        for (id, pending) in &inner.approvals {
            let Some((session, path)) = request(pending) else { continue };
            if !inner.live_thread(session)
                || !canonical_folder(Path::new(path), &protected).is_ok_and(|folder| folder == path)
                || store.active(session, now, inner.current_turn(session)).count()
                    + grants.iter().filter(|grant: &&FolderGrant| grant.session == session).count() >= 100
            {
                invalid.push(id.clone());
                continue;
            }
            grants.push(FolderGrant {
                id: id.clone(),
                session: session.into(),
                path: path.into(),
                expiry: FolderGrantExpiry::OneHour,
                expires_at: Some(now + 60 * 60 * 1000),
                granted_by: "full_mode".into(),
            });
        }
        (grants, invalid)
    }

    pub(super) fn apply_folder_sweep(&self, inner: &mut Inner, grants: &[FolderGrant]) -> Result<Vec<String>> {
        let protected = inner.folder_store()?.protected();
        let now = chrono::Utc::now().timestamp_millis();
        let live: Vec<_> = grants.iter().filter(|grant| {
            inner.live_thread(&grant.session)
                && grant.expires_at.is_none_or(|expiry| now < expiry)
                && canonical_folder(Path::new(&grant.path), &protected).is_ok_and(|folder| folder == grant.path)
        }).collect();
        inner.folder_store_mut()?.apply_full_sweep(&live)?;
        Ok(live.into_iter().map(|grant| grant.id.clone()).collect())
    }

    /// Issue a one-hour grant for the exact folder of the current read. The worker
    /// caller receives this ID for its retry; its assignment grants stay unchanged.
    pub(crate) fn auto_folder_access(&self, session: &str, job: Option<&str>, path: &Path) -> Result<Option<String>> {
        let mut inner = self.inner.lock().unwrap();
        self.auto_folder_access_locked(&mut inner, session, job, path)
    }

    fn auto_folder_access_locked(&self, inner: &mut Inner, session: &str, job: Option<&str>, path: &Path) -> Result<Option<String>> {
        if crate::canary() {
            return Ok(None);
        }
        self.finish_folder_sweep_locked(inner)?;
        let Some((mode, changes)) = self.full_approval_lease()? else { return Ok(None) };
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let folder = canonical_folder(path, &inner.folder_store()?.protected())?;
        let turn = inner.current_turn(session);
        let grant = inner.folder_store()?.active(session, chrono::Utc::now().timestamp_millis(), turn)
            .find(|grant| grant.path == folder && grant.granted_by == "full_mode");
        let id = if let Some(grant) = grant {
            // Old grants remain available to their existing holders. An active mode
            // is required to give the current caller a new reference to one.
            if changes.has_changed().unwrap_or(true)
                || mode.expires_at.is_some_and(|at| at <= chrono::Utc::now().timestamp_millis())
            {
                return Ok(None);
            }
            grant.id.clone()
        } else {
            let full = full_registration(session, job, &folder, &crate::mail::session_key());
            if !crate::full_approval::authorize(self, full)? {
                return Ok(None);
            }
            self.add_folder_grant_locked(inner, session, folder, FolderGrantExpiry::OneHour, "full_mode")?.id
        };
        Ok(Some(id))
    }

    pub(crate) fn request_folder_access(&self, session: &str, path: &Path) -> Result<AccessRequest> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = self.auto_folder_access_locked(&mut inner, session, None, path)? {
            return Ok(AccessRequest::Granted(id));
        }
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let folder = canonical_folder(path, &inner.folder_store()?.protected())?;
        let mut pending = 0;
        for (id, approval) in &inner.approvals {
            if let Some((_, path)) = request(approval).filter(|(owner, _)| *owner == session) {
                if path == folder {
                    return Ok(AccessRequest::Pending(id.clone()));
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
        Ok(AccessRequest::Pending(request_id))
    }

    pub(crate) fn decide_folder_access(
        &self,
        request_id: &str,
        expiry: Option<FolderGrantExpiry>,
        principal: &str,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.finish_folder_sweep_locked(&mut inner)?;
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
        self.add_folder_grant_locked(&mut inner, session, folder, expiry, principal).map(|_| ())
    }

    fn add_folder_grant_locked(
        &self,
        inner: &mut Inner,
        session: &str,
        path: String,
        expiry: FolderGrantExpiry,
        principal: &str,
    ) -> Result<FolderGrant> {
        ensure!(inner.live_thread(session), "Thread is archived or missing");
        let turn = inner.current_turn(session);
        let grant = inner
            .folder_store_mut()?
            .add(session.into(), path, expiry, turn, principal)?;
        let _ = self
            .tx
            .send(crate::snapshot(ServerEvent::FolderGrantsChanged {
                session: session.into(),
            }));
        Ok(grant)
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
        let mut inner = self.inner.lock().unwrap();
        self.finish_folder_sweep_locked(&mut inner)?;
        inner.folder_store_mut()?.revoke(session, id)?;
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
        self.finish_folder_sweep_locked(inner)?;
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
            self.withdraw_approvals_for(session, None, "thread archived")?;
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
