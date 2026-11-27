//! Test discovery and runs inside a delegated job's own checkout, including
//! isolated copies below it with a private build directory.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use fuzukue_proto::{ApprovalOwner, ApprovalRisk, LocalizedMessage};
use serde::Deserialize;
use serde_json::{Value, json};

use super::parse_args;
use crate::{project_tests::Planned, Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    dir: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    suite: Option<String>,
    filter: Option<String>,
    dir: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
}

/// An existing or new directory inside the checkout, or inside the job's
/// temporary directory when given as an absolute path there. No links.
fn directory(checkout: &Path, scratch: &Path, path: &Path, create: bool) -> Result<PathBuf> {
    let (base, relative) = match path.strip_prefix(scratch).or_else(|_| path.strip_prefix(checkout)) {
        Ok(relative) if path.starts_with(scratch) => (scratch, relative),
        Ok(relative) => (checkout, relative),
        Err(_) => (checkout, path),
    };
    ensure!(
        relative.components().all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
            && !relative.components().any(|part| part.as_os_str().eq_ignore_ascii_case(".git")),
        "Use a directory inside the checkout, or an absolute path inside the job's temporary directory"
    );
    let mut directory = base.to_owned();
    for part in relative.components().filter(|part| matches!(part, Component::Normal(_))) {
        directory.push(part);
        match directory.symlink_metadata() {
            Ok(metadata) => ensure!(metadata.is_dir(), "{} is not a plain directory", directory.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => std::fs::create_dir(&directory)?,
            Err(error) => return Err(error).with_context(|| format!("Could not open {}", directory.display())),
        }
    }
    Ok(directory)
}

fn roots(app: &Workspace, job: &str) -> Result<(PathBuf, PathBuf)> {
    let storage = app.delegates.capture_scope(job)?;
    Ok((storage.cwd, storage.scratch))
}

pub(super) async fn list(app: &Workspace, job: &str, args: &Value) -> Result<Value> {
    let args: ListArgs = parse_args(args)?;
    let (checkout, scratch) = roots(app, job)?;
    let root = directory(&checkout, &scratch, args.dir.as_deref().unwrap_or(Path::new(".")), false)?;
    Ok(json!({"directory": root, "suites": crate::project_tests::suites_in(&root).await?}))
}

pub(super) async fn run(app: &Workspace, session: &str, job: &str, args: &Value) -> Result<Value> {
    let args: RunArgs = parse_args(args)?;
    let (checkout, scratch) = roots(app, job)?;
    let (_, cancel) = app.delegates.workspace_write_scope(session, job)?;
    let root = directory(&checkout, &scratch, args.dir.as_deref().unwrap_or(Path::new(".")), false)?;
    // Tests run the checkout's code with the server's permissions, so this
    // follows the same decision as a shell command.
    let mut registration = crate::full_approval::registration(
        ApprovalOwner { thread: session.into(), job: job.into(), requester: "worker".into() },
        "run_tests",
        ApprovalRisk::High,
        json!({"request": crate::mail::session_key()}),
        json!({"directory": root, "suite": args.suite}),
    );
    if !app.delegates.worker_approval_trusted(session, job) {
        crate::full_approval::require_person(&mut registration);
    }
    let planned = Planned::discover(&root, args.suite.as_deref()).await?;
    // A configured command is checked like a shell command.
    if planned.guarded(&crate::worker_scope::protected_roots(&app.state_dir)).is_some() {
        crate::full_approval::require_person(&mut registration);
    }
    let summary = LocalizedMessage::new("approval-worker-tests", [
        ("directory", root.display().to_string()),
        ("commands", planned.card()),
    ]);
    let decision = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(crate::cancellation::Cancelled.into()),
        decision = crate::delegate::request_workspace_write_approval(app, session, job, summary, registration) => decision?,
    };
    ensure!(decision == "accept", "The test run was declined");
    let tmp = match &args.tmp_dir {
        Some(path) => directory(&checkout, &scratch, path, true)?,
        None => scratch.clone(),
    };
    let tmp = tmp.to_str().context("The temporary directory must be UTF-8")?.to_owned();
    let mut env: BTreeMap<String, String> = ["TMPDIR", "TMP", "TEMP"].map(|name| (name.into(), tmp.clone())).into();
    if let Some(path) = &args.target_dir {
        let target = directory(&checkout, &scratch, path, true)?;
        env.insert("CARGO_TARGET_DIR".into(), target.to_str().context("The build directory must be UTF-8")?.into());
        for name in ["CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"] {
            env.insert(name.into(), "0".into());
        }
    }
    let _work = app.admit_work()?;
    let runs = crate::project_tests::run_isolated(&root, planned, args.filter.as_deref(), &env, &cancel).await?;
    let (mut passed, mut failed, mut budget) = (0, 0, 5);
    let runs: Vec<_> = runs.into_iter().map(|run| {
        passed += run.passed;
        failed += run.failed;
        let failures: Vec<_> = run.failures.into_iter().take(budget).collect();
        budget -= failures.len();
        json!({"suite": run.suite, "state": run.state, "passed": run.passed, "failed": run.failed, "failures": failures})
    }).collect();
    Ok(json!({"directory": root, "environment": env, "passed": passed, "failed": failed, "runs": runs}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn suite_commands_are_checked_shown_and_run_with_a_small_environment() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        std::fs::write(root.join("fuzukue.toml"), r#"
[tests.publish]
command = ["bash", "-c", "git push"]
env = { MODE = "x" }

[tests.env]
command = "env"
"#)?;
        let publish = Planned::discover(&root, Some("publish")).await?;
        assert_eq!(publish.guarded(&[]), Some(crate::full_approval::guard::Reason::ExternalWrite));
        assert!(publish.card().contains("git push") && publish.card().contains("MODE="), "{}", publish.card());
        let all = Planned::discover(&root, None).await?;
        assert!(all.guarded(&[]).is_some() && all.card().lines().count() == 2);

        let plain = Planned::discover(&root, Some("env")).await?;
        assert_eq!(plain.guarded(&[]), None);
        // Cargo sets this for the test process, and the child must not inherit it.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let env = [("EXTRA".to_owned(), "kept".to_owned())].into();
        let runs = crate::project_tests::run_isolated(&root, plain, None, &env, &Default::default()).await?;
        let output = &runs[0].output;
        assert!(!output.contains("CARGO_MANIFEST_DIR") && output.contains("EXTRA=kept"), "{output}");
        Ok(())
    }

    #[test]
    fn directories_stay_inside_the_checkout_or_job_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let (checkout, scratch) = (base.join("checkout"), base.join("scratch"));
        std::fs::create_dir(&checkout)?;
        std::fs::create_dir(&scratch)?;
        std::os::unix::fs::symlink(&base, checkout.join("escape"))?;
        assert_eq!(directory(&checkout, &scratch, Path::new("."), false)?, checkout);
        assert_eq!(directory(&checkout, &scratch, Path::new("target/copy/build"), true)?, checkout.join("target/copy/build"));
        assert_eq!(directory(&checkout, &scratch, &scratch.join("tmp"), true)?, scratch.join("tmp"));
        assert_eq!(directory(&checkout, &scratch, &checkout.join("target"), false)?, checkout.join("target"));
        for path in [Path::new("../outside"), Path::new("escape/outside"), Path::new("/etc"), Path::new(".git/x"), &base] {
            assert!(directory(&checkout, &scratch, path, true).is_err(), "{}", path.display());
        }
        assert!(!base.join("outside").exists());
        assert!(directory(&checkout, &scratch, Path::new("missing"), false).is_err());
        Ok(())
    }
}
