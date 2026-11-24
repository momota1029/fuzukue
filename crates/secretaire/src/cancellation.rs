use anyhow::{Context, Result};
use rustix::process::{Pid, Signal, kill_process_group};
use std::process::{Output, Stdio};
use tokio::process::{Child, Command};
use tokio::sync::watch;

pub(crate) struct Cancellation(watch::Sender<bool>);

#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("止めた")
    }
}

impl std::error::Error for Cancelled {}

impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::Sender::new(false))
    }
}

impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn check(&self) -> Result<()> {
        if *self.0.borrow() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    pub(crate) async fn cancelled(&self) {
        let _ = self.0.subscribe().wait_for(|stopped| *stopped).await;
    }
}

/// Kill the command's helpers and hooks as well when its future is dropped.
pub(crate) struct ProcessGroup(Option<Pid>);

impl ProcessGroup {
    pub(crate) fn new(child: &Child) -> Result<Self> {
        let pid = child.id().and_then(|id| Pid::from_raw(id as i32)).context("Command has no valid PID")?;
        Ok(Self(Some(pid)))
    }

    pub(crate) fn finished(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

pub(crate) async fn command_output(mut command: Command) -> Result<Output> {
    let child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .process_group(0).kill_on_drop(true).spawn()?;
    let mut group = ProcessGroup::new(&child)?;
    let output = child.wait_with_output().await?;
    group.finished();
    Ok(output)
}
