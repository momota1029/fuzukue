use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use crate::delegate::touches::WriteSet;
use crate::mcp::workspace_read::git_read::tests::{Fixture, git};

const JOB: &str = "delegate:git-commit-test";

fn assigned_scope(root: &Path, state: &Path, paths: &[&str]) -> Result<Scope> {
    fs::create_dir_all(state)?;
    let writes = Arc::new(WriteSet::new(
        root,
        Some(paths.iter().map(|path| (*path).into()).collect()),
        "",
        || None,
    )?);
    Scope::new(root.canonicalize()?, writes, state, JOB)
}

fn clone_repo(fixture: &Fixture) -> Result<PathBuf> {
    let clone = fixture.temp.path().join("private-clone");
    git(
        fixture.temp.path(),
        &[
            "clone",
            "--no-local",
            fixture.repo.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    )?;
    Ok(clone)
}

async fn run_commit(
    root: &Path,
    state: &Path,
    scope: &Scope,
    message: &str,
    paths: &[&str],
) -> Result<Value> {
    commit(
        root,
        state,
        scope,
        &Cancellation::default(),
        Args {
            message: message.into(),
            paths: paths.iter().map(|path| (*path).into()).collect(),
        },
    )
    .await
}

#[tokio::test]
async fn commits_only_named_files_preserves_message_and_existing_staging_and_skips_hooks()
-> Result<()> {
    let fixture = Fixture::new()?;
    let clone = clone_repo(&fixture)?;
    let state = fixture.temp.path().join("state");
    let scope = assigned_scope(&clone, &state, &["tracked.txt", "new.txt"])?;
    git(&clone, &["config", "user.name", "Local Test Identity"])?;
    git(
        &clone,
        &["config", "user.email", "local-test@example.invalid"],
    )?;
    git(&clone, &["config", "commit.gpgSign", "true"])?;
    fs::write(clone.join("unrelated.txt"), "already staged\n")?;
    git(&clone, &["add", "unrelated.txt"])?;

    let hooks = clone.join(".git/hooks");
    for name in ["pre-commit", "commit-msg", "reference-transaction"] {
        let hook = hooks.join(name);
        fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n")?;
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755))?;
    }
    git(
        &clone,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    )?;
    fs::write(clone.join("tracked.txt"), "changed\n")?;
    fs::write(clone.join("new.txt"), "brand new\n")?;
    let message = "  Scoped change  \n\n# A literal comment\n\nCo-Authored-By: Test Contributor <contributor@example.invalid>\n\n";
    let result = run_commit(
        &fixture.repo,
        &state,
        &scope,
        message,
        &["tracked.txt", "new.txt"],
    )
    .await?;
    assert_eq!(result["created"], true);
    assert_eq!(result["paths"], json!(["new.txt", "tracked.txt"]));
    let commit = git(&clone, &["rev-parse", "HEAD"])?;
    assert_eq!(result["commit_sha"], commit);
    assert_eq!(
        result["tree_sha"],
        git(&clone, &["rev-parse", "HEAD^{tree}"])?
    );
    assert_eq!(
        git(&clone, &["diff", "--cached", "--name-only"])?,
        "unrelated.txt"
    );
    assert_eq!(git(&clone, &["show", "HEAD:new.txt"])?, "brand new");
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%an <%ae>"])?,
        "Local Test Identity <local-test@example.invalid>"
    );
    let raw = git_read::output({
        let mut command = Command::new("git");
        command
            .current_dir(&clone)
            .args(["cat-file", "commit", &commit])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    })
    .await?;
    assert!(raw.bytes.ends_with(message.as_bytes()));
    assert!(!clone.join("hook-ran").exists());
    assert!(!clone.join(".git/index.lock").exists());
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
    assert!(git(&fixture.repo, &["status", "--porcelain"])?.is_empty());

    let audit = crate::mcp::workspace_write::audit_page(&state, JOB, None, 10)?;
    assert_eq!(audit["writes"][0]["outcome"], "written");
    assert_eq!(audit["writes"][0]["tool"], NAME);
    assert_eq!(audit["writes"][0]["commit_sha"], commit);
    assert_eq!(audit["writes"][0]["paths"], result["paths"]);

    let noop = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "No changes",
        &["tracked.txt"],
    )
    .await?;
    assert_eq!(noop["created"], false);
    assert!(noop["message"].as_str().unwrap().contains("No changes"));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"])?, commit);
    assert_eq!(
        git(&clone, &["diff", "--cached", "--name-only"])?,
        "unrelated.txt"
    );

    fs::remove_file(clone.join("tracked.txt"))?;
    let deletion = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "Record deletion",
        &["tracked.txt"],
    )
    .await?;
    assert_eq!(deletion["paths"], json!(["tracked.txt"]));
    assert_eq!(
        git(&clone, &["show", "--format=", "--name-status", "HEAD"])?,
        "D\ttracked.txt"
    );
    assert_eq!(
        git(&clone, &["diff", "--cached", "--name-only"])?,
        "unrelated.txt"
    );
    Ok(())
}

#[tokio::test]
async fn refuses_outside_paths_and_missing_identity_and_honors_server_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    let clone = clone_repo(&fixture)?;
    let state = fixture.temp.path().join("state");
    let scope = assigned_scope(
        &clone,
        &state,
        &["tracked.txt", "alias", ".git/config", ".env"],
    )?;
    fs::write(clone.join("tracked.txt"), "changed\n")?;
    fs::write(clone.join("out-of-scope.txt"), "outside touches\n")?;
    fs::write(clone.join(".env"), "fixture\n")?;
    symlink(fixture.repo.join("tracked.txt"), clone.join("alias"))?;
    for path in [
        "out-of-scope.txt",
        "alias",
        ".git/config",
        ".env",
        "../repo/tracked.txt",
        "sub/../tracked.txt",
    ] {
        let err = run_commit(&fixture.repo, &state, &scope, "Should fail", &[path])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("path") || err.to_string().contains("links"),
            "{path}: {err:#}"
        );
    }
    let err = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "No local identity",
        &["tracked.txt"],
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("local Git user.name and user.email"),
        "{err:#}"
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"])?, fixture.head);
    assert!(git(&clone, &["diff", "--cached", "--name-only"])?.is_empty());
    assert!(!clone.join(".git/index.lock").exists());

    let desk = assigned_scope(&fixture.repo, &state, &["tracked.txt"])?;
    let err = run_commit(
        &fixture.repo,
        &state,
        &desk,
        "Cannot commit at the front desk",
        &["tracked.txt"],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("separate worker checkout"));

    git(&clone, &["config", "user.name", "Another Identity"])?;
    git(
        &clone,
        &["config", "user.email", "nonserver-test@example.invalid"],
    )?;
    fs::write(
        state.join("fuzukue.toml"),
        "[delegate.git_commit]\nname = 'Server Test Identity'\nemail = 'server-test@example.invalid'\n",
    )?;
    let result = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "Use server identity",
        &["tracked.txt"],
    )
    .await?;
    assert_eq!(result["created"], true);
    assert_eq!(
        git(&clone, &["log", "-1", "--format=%an <%ae>"])?,
        "Server Test Identity <server-test@example.invalid>"
    );
    Ok(())
}

#[tokio::test]
async fn linked_worktree_cannot_commit_to_the_front_desks_branch() -> Result<()> {
    let fixture = Fixture::new()?;
    let linked = fixture.temp.path().join("linked");
    git(
        &fixture.repo,
        &["worktree", "add", "-b", "worker", linked.to_str().unwrap()],
    )?;
    let state = fixture.temp.path().join("state");
    let scope = assigned_scope(&linked, &state, &["tracked.txt"])?;
    fs::write(linked.join("tracked.txt"), "linked change\n")?;
    let result = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "Commit to worker branch",
        &["tracked.txt"],
    )
    .await?;
    assert_eq!(result["created"], true);
    assert_eq!(
        git(&linked, &["rev-parse", "HEAD"])?,
        result["commit_sha"].as_str().unwrap()
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
    assert!(git(&fixture.repo, &["status", "--porcelain"])?.is_empty());

    let (metadata, _) = git_metadata(&linked)?;
    fs::write(metadata.join("HEAD"), "ref: refs/heads/main\n")?;
    fs::write(linked.join("tracked.txt"), "must not commit to main\n")?;
    let refused = run_commit(
        &fixture.repo,
        &state,
        &scope,
        "Forbidden branch",
        &["tracked.txt"],
    )
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains("front desk's active branch"),
        "{refused:#}"
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"])?, fixture.head);
    Ok(())
}

#[test]
fn mcp_commit_is_worker_only_and_scoped_for_preapproval() {
    use crate::mcp;

    let schema = definition();
    assert_eq!(
        schema["inputSchema"]["required"],
        json!(["message", "paths"])
    );
    assert_eq!(schema["inputSchema"]["additionalProperties"], false);
    assert!(
        serde_json::from_value::<Args>(json!({"message":"test", "paths":["a"], "amend":true}))
            .is_err()
    );
    let worker = mcp::worker_tool_names(&[]);
    assert!(worker.iter().any(|name| name == NAME));
    assert!(!worker.iter().any(|name| name == "git_commit"));
    assert!(
        !mcp::worker_tool_names(&["fs_write".into()])
            .iter()
            .any(|name| name == NAME)
    );
    assert_eq!(
        mcp::codex_tool_policy(Some(&worker))[NAME]["approval_mode"],
        "approve"
    );
    assert!(mcp::codex_tool_policy(None).get(NAME).is_none());
    assert!(!mcp::claude_tool_names(false, false).any(|name| name.ends_with(NAME)));

    let desk = mcp::partner_tool_list(false);
    let desk = desk["tools"].as_array().unwrap();
    assert!(desk.iter().any(|tool| tool["name"] == "git_commit"));
    assert!(!desk.iter().any(|tool| tool["name"] == NAME));
}
