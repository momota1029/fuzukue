//! ワークスペースの Lean 検査。開いたときと手動の検査を直列化し、画面へ配る。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fuzukue_proto::ServerEvent;

use super::{Check, Failures, Run, Statuses};
use crate::Workspace;

const INTERRUPTED: &str = "error: サーバーの終了・更新のため検査を中断した。受け付けを再開したら残りを確かめる";

#[derive(Default)]
pub(crate) struct State {
    run: tokio::sync::Mutex<()>,
    open_queued: AtomicBool,
    resume_pending: AtomicBool,
    failures: std::sync::Mutex<Failures>,
    #[cfg(test)]
    pub(crate) lake: std::sync::OnceLock<Vec<std::ffi::OsString>>,
}

impl Workspace {
    /// 保存した結果を出し、入力が変わったパッケージだけを確かめる。
    pub(crate) fn check_lean_on_open(self: &Arc<Self>) {
        // A workspace with no editor attached does not need to build Lean packages.
        if !crate::canary() && !self.lean.open_queued.swap(true, Ordering::AcqRel) {
            self.check_lean(Run::Open);
        }
    }

    /// サーバーの終了や更新が始まると偽になる。検査は受け付けの門を持ち続けない。
    fn lean_admitted(&self) -> bool {
        self.update().is_none_or(|update| update.admission_open())
    }

    pub(crate) fn check_lean(
        self: &Arc<Self>,
        run: Run,
    ) -> tokio::task::JoinHandle<anyhow::Result<Vec<(String, String)>>> {
        let app = self.clone();
        tokio::spawn(async move {
            let guard = app.lean.run.lock().await;
            let result = app.run_lean(run).await;
            if run == Run::Open {
                app.lean.open_queued.store(false, Ordering::Release);
            }
            drop(guard);
            let (decls, interrupted) = result?;
            if interrupted {
                app.resume_lean();
            }
            Ok(decls)
        })
    }

    async fn run_lean(self: &Arc<Self>, run: Run) -> anyhow::Result<(Vec<(String, String)>, bool)> {
        let current = self.lean_status.lock().unwrap().clone();
        let findings = self.lean_doc_findings.lock().unwrap().clone();
        self.publish_lean(current, true, findings);
        let doc_findings = crate::lean_reading::doc_check(&self.project_dir)
            .await
            .map(|check| check.findings)
            .unwrap_or_default();
        #[cfg(not(test))]
        let lake = vec!["lake".into()];
        // Tests can substitute a synthetic Lake.
        #[cfg(test)]
        let lake = self.lean.lake.get().cloned().unwrap_or_else(|| vec!["lake".into()]);
        let admitted = || self.lean_admitted();
        let mut progress = |statuses: &Statuses| {
            self.publish_lean(rows(statuses), true, doc_findings.clone());
        };
        let result = super::check(Check {
            project: &self.project_dir,
            state: &self.state_dir,
            reading: true,
            run,
            lake: &lake,
            failures: &self.lean.failures,
            admitted: &admitted,
            progress: &mut progress,
        })
        .await;
        let (decls, result) = match result {
            Ok(mut outcome) => {
                if outcome.interrupted {
                    outcome.statuses.insert("(lean)".into(), INTERRUPTED.into());
                }
                let decls = rows(&outcome.statuses);
                (decls.clone(), Ok((decls, outcome.interrupted)))
            }
            Err(error) => (vec![("(lean)".into(), format!("error: {error:#}"))], Err(error)),
        };
        self.publish_lean(decls, false, doc_findings);
        result
    }

    fn publish_lean(
        &self,
        decls: Vec<(String, String)>,
        running: bool,
        doc_findings: Vec<fuzukue_proto::LeanDocFinding>,
    ) {
        *self.lean_status.lock().unwrap() = decls.clone();
        *self.lean_doc_findings.lock().unwrap() = doc_findings.clone();
        self.hub.publish_snapshot(ServerEvent::LeanStatus { decls, running, doc_findings });
    }

    /// 中断した検査は、受け付けが戻ったら（取り消された終了など）続きから始める。
    fn resume_lean(self: &Arc<Self>) {
        if self.lean.resume_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let workspace = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let Some(workspace) = workspace.upgrade() else { break };
                if workspace.lean_admitted() {
                    workspace.lean.resume_pending.store(false, Ordering::Release);
                    workspace.check_lean_on_open();
                    break;
                }
            }
        });
    }
}

fn rows(statuses: &Statuses) -> Vec<(String, String)> {
    statuses.iter().map(|(name, state)| (name.clone(), state.clone())).collect()
}
