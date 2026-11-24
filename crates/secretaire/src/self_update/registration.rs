use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    CommandLine, PREBUILT, State, Status, Step, Update, WINDOWS_BUILD_STATE, WINDOWS_PUBLISH,
    write_json, write_status,
};

const DIRECTORY: &str = "client-registration";
const REQUEST: &str = "request.json";
const ACCEPTED: &str = "accepted.json";

#[derive(Deserialize, Serialize)]
struct Registration {
    project: PathBuf,
    prebuilt: bool,
    commands: Vec<CommandLine>,
    next: usize,
}

pub(super) fn accept(candidate: &Path, status: &mut Status) {
    let Some(step) = status
        .steps
        .iter_mut()
        .find(|step| step.kind == Step::Publish && step.state == State::Pending)
    else {
        return;
    };
    if let Err(error) = release(candidate) {
        step.state = State::Failed;
        status.error = Some(format!(
            "Server swap succeeded, but client registration could not be released: {error:#}"
        ));
    }
}

fn release(candidate: &Path) -> Result<()> {
    let directory = candidate
        .parent()
        .context("missing candidate directory")?
        .join(DIRECTORY);
    // Only the supervisor that verified the new instance can release this request.
    match fs::rename(directory.join(REQUEST), directory.join(ACCEPTED)) {
        Ok(()) => File::open(directory)?.sync_all().map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn discard(candidate: &Path, status: &mut Status) {
    let Some(step) = status
        .steps
        .iter_mut()
        .find(|step| step.kind == Step::Publish && step.state == State::Pending)
    else {
        return;
    };
    step.state = State::Skipped;
    if let Some(parent) = candidate.parent() {
        if let Err(error) = fs::remove_dir_all(parent.join(DIRECTORY)) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("secretaire self-swap: remove client registration: {error:#}");
            }
        }
    }
}

impl Update {
    pub(super) fn queue_registration(
        &self,
        checkout: tempfile::TempDir,
        candidate: &Path,
    ) -> Result<PathBuf> {
        let parent = candidate.parent().context("missing candidate directory")?;
        let staging = tempfile::tempdir_in(parent)?;
        let registration = Registration {
            project: self.project.clone(),
            prebuilt: !self.config.prebuild().is_empty(),
            commands: self.config.publish.clone(),
            next: 0,
        };
        fs::rename(checkout.path(), staging.path().join("checkout"))?;
        write_json(&staging.path().join(REQUEST), &registration)?;
        let directory = parent.join(DIRECTORY);
        fs::rename(staging.path(), &directory)?;
        File::open(parent)?.sync_all()?;
        Ok(directory)
    }

    pub(super) async fn finish_registration(
        &self,
        distribution: &crate::distribution::Distribution,
    ) -> Result<()> {
        let Some(parent) = self.exe.parent() else {
            return Ok(());
        };
        let directory = parent.join(DIRECTORY);
        let accepted = directory.join(ACCEPTED);
        if self.trial_pending() || !accepted.is_file() {
            return Ok(());
        }
        // The supervisor may still be releasing the lock after removing its health marker.
        let _lock = loop {
            let lock = File::open(self.root.join("lock"))?;
            match lock.try_lock() {
                Ok(()) => break lock,
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        if fs::canonicalize(self.root.join("current")).ok().as_deref() != Some(&self.exe)
            || *self.swapping.borrow()
        {
            return Ok(());
        }
        let input = match File::open(&accepted) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut registration: Registration = serde_json::from_reader(input)?;
        ensure!(
            registration.project == self.project,
            "pending client registration belongs to a different project"
        );

        self.status.send_modify(|status| {
            status.active = true;
            status.error = None;
            status.canary_status = None;
        });
        self.step(Step::Swap, State::Passed);
        self.step(Step::Publish, State::Running);
        let result = self
            .register(&directory.join("checkout"), &accepted, &mut registration)
            .await;
        match result {
            Ok(()) => {
                self.step(Step::Publish, State::Passed);
                self.status.send_modify(|status| status.active = false);
                distribution.announce_release();
                write_status(&self.root, &self.status.borrow())?;
                fs::remove_dir_all(directory)?;
            }
            Err(error) => {
                let message = format!(
                    "Server swap succeeded, but client registration failed. It will retry on server startup: {error:#}"
                );
                self.step(Step::Publish, State::Failed);
                self.status.send_modify(|status| {
                    status.active = false;
                    status.error = Some(message.clone());
                });
                write_status(&self.root, &self.status.borrow())?;
                anyhow::bail!("{message}");
            }
        }
        Ok(())
    }

    async fn register(
        &self,
        checkout: &Path,
        accepted: &Path,
        registration: &mut Registration,
    ) -> Result<()> {
        while let Some(line) = registration.commands.get(registration.next) {
            let mut command = if registration.prebuilt && line.is_default_windows_publish() {
                CommandLine::Argv(vec![WINDOWS_PUBLISH.into(), "register".into()]).command()?
            } else {
                line.command()?
            };
            command.env_remove(PREBUILT).env_remove(WINDOWS_BUILD_STATE);
            if registration.prebuilt {
                command
                    .env(PREBUILT, "1")
                    .env(WINDOWS_BUILD_STATE, checkout.join(".windows-build"));
            }
            self.command(Step::Publish, checkout, &mut command, Some(&self.exe))
                .await?;
            registration.next += 1;
            write_json(accepted, registration)?;
        }
        Ok(())
    }
}
