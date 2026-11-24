use super::*;
use anyhow::ensure;

pub(in crate::mcp) fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(in crate::mcp) struct Fixture {
    pub(in crate::mcp) temp: tempfile::TempDir,
    pub(in crate::mcp) repo: PathBuf,
    pub(in crate::mcp) head: String,
}

impl Fixture {
    pub(in crate::mcp) fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        fs::create_dir(&repo)?;
        git(&repo, &["init", "-b", "main"])?;
        git(&repo, &["config", "user.name", "Git Read Test"])?;
        git(&repo, &["config", "user.email", "git-read@example.invalid"])?;
        fs::write(repo.join("tracked.txt"), "first\nsecond\n")?;
        git(&repo, &["add", "tracked.txt"])?;
        git(&repo, &["commit", "-m", "initial"])?;
        git(&repo, &["tag", "v1"])?;
        let head = git(&repo, &["rev-parse", "HEAD"])?;
        Ok(Self { temp, repo, head })
    }

    fn scope(&self) -> Result<ReadScope> {
        ReadScope::new(&self.repo, &self.temp.path().join("state"), None)
    }
}

async fn run(scope: &ReadScope, subcommand: &str, args: &[&str]) -> Result<Value> {
    call(
        scope.clone(),
        &json!({"subcommand": subcommand, "args": args}),
    )
    .await
}

#[test]
fn only_allowlisted_queries_and_literal_paths() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.scope()?;
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside)?;
    symlink(&outside, f.repo.join("link"))?;
    let accepted: &[(&str, &[&str])] = &[
        ("rev-parse", &[]),
        ("rev-parse", &["HEAD"]),
        ("rev-parse", &["--show-toplevel"]),
        ("rev-parse", &["--is-inside-work-tree"]),
        ("rev-parse", &["--abbrev-ref", "HEAD"]),
        ("rev-parse", &["--verify", "HEAD^{tree}"]),
        ("status", &["--porcelain"]),
        ("log", &["-n", "100", "HEAD~1"]),
        ("diff", &["--cached", "--", "tracked.txt"]),
        ("diff", &["HEAD~1", "HEAD", "--", "filename with spaces"]),
        ("show", &["HEAD", "--", "tracked.txt"]),
        ("show", &["--stat", "HEAD", "--", "tracked.txt"]),
        ("diff", &["--stat", "--cached", "--", "tracked.txt"]),
        ("ls-files", &[]),
        ("ls-files", &["--", "literal*[file].txt"]),
        ("ls-tree", &["-r", "HEAD"]),
        ("cat-file", &["-t", "main^{commit}"]),
        ("cat-file", &["-s", "HEAD"]),
        ("cat-file", &["-p", "refs/heads/main"]),
        ("merge-base", &["HEAD", "main"]),
        ("merge-base", &["--is-ancestor", "main", "HEAD"]),
        ("describe", &["--tags", "--always"]),
        ("branch", &["--list"]),
        ("tag", &["--list"]),
        ("remote", &["-v"]),
        ("blame", &["-L", "1,2", "HEAD", "--", "tracked.txt"]),
        ("config", &["--get", "core.filemode"]),
    ];
    let rejected: &[(&str, &[&str])] = &[
        ("fetch", &[]),
        ("alias", &[]),
        ("--git-dir=outside", &[]),
        ("rev-parse", &["--git-dir=outside"]),
        ("rev-parse", &["--work-tree=/"]),
        ("rev-parse", &["--exec-path"]),
        ("rev-parse", &["--verify", "HEAD:../file"]),
        ("rev-parse", &["--verify", "-c", "core.pager=foo"]),
        ("rev-parse", &["@{upstream}"]),
        ("status", &["--porcelain", "--ignored"]),
        ("status", &["--"]),
        ("log", &["--format=%B"]),
        ("log", &["-n", "101"]),
        ("log", &["--show-signature"]),
        ("diff", &["--output=target"]),
        ("diff", &["--ext-diff"]),
        ("diff", &["--stat", "--ext-diff"]),
        ("show", &["--stat", "--textconv"]),
        ("diff", &["--no-index", "a", "b"]),
        ("diff", &["--", "../x"]),
        ("diff", &["--", "a/../x"]),
        ("diff", &["--", "/outside"]),
        ("diff", &["--", ":(top)other"]),
        ("show", &["--textconv", "HEAD"]),
        ("show", &["--", "link/file"]),
        ("show", &["--", ".git/config"]),
        ("ls-tree", &["HEAD", "--", "a\\..\\x"]),
        ("ls-files", &["--", ".env"]),
        ("ls-files", &["--recurse-submodules"]),
        ("cat-file", &["--batch"]),
        ("cat-file", &["-p", "HEAD:path"]),
        ("merge-base", &["--octopus", "a", "b"]),
        ("describe", &["--all"]),
        ("branch", &["-D", "main"]),
        ("tag", &["-d", "v1"]),
        ("remote", &["show", "--upload-pack=other"]),
        ("remote", &["set-url", "origin", "new"]),
        ("blame", &["-L", "/pattern/,2", "--", "tracked.txt"]),
        ("blame", &["-L", "1,1001", "--", "tracked.txt"]),
        ("config", &["--get", "remote.origin.url"]),
        ("config", &["core.pager", "helper"]),
    ];
    for &(subcommand, args) in accepted {
        assert!(
            Args {
                subcommand: subcommand.into(),
                args: args.iter().map(|s| (*s).into()).collect()
            }
            .command(&scope)
            .is_ok(),
            "{subcommand} {args:?}"
        );
    }
    for &(subcommand, args) in rejected {
        assert!(
            Args {
                subcommand: subcommand.into(),
                args: args.iter().map(|s| (*s).into()).collect()
            }
            .command(&scope)
            .is_err(),
            "{subcommand} {args:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn commands_work_in_a_checkout_and_are_bounded() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.scope()?;
    let tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"])?;
    let cases: &[(&str, &[&str], &str)] = &[
        ("rev-parse", &["HEAD"], &f.head),
        ("rev-parse", &["--abbrev-ref", "HEAD"], "main"),
        ("rev-parse", &["--verify", "HEAD^{tree}"], &tree),
        ("rev-parse", &["--show-toplevel"], f.repo.to_str().unwrap()),
        ("rev-parse", &["--is-inside-work-tree"], "true"),
        ("status", &["--porcelain"], ""),
        ("log", &["-n", "1"], "initial"),
        ("show", &["HEAD", "--", "tracked.txt"], "+first"),
        ("show", &["--stat", "HEAD"], "1 file changed, 2 insertions(+)"),
        ("ls-files", &[], "tracked.txt"),
        ("ls-tree", &["HEAD"], "tracked.txt"),
        ("cat-file", &["-t", "HEAD"], "commit"),
        ("cat-file", &["-s", "HEAD"], ""),
        (
            "cat-file",
            &["-p", &git(&f.repo, &["rev-parse", "HEAD:tracked.txt"])?],
            "first",
        ),
        ("merge-base", &["main", "HEAD"], &f.head),
        ("describe", &["--tags"], "v1"),
        ("branch", &["--list"], "main"),
        ("tag", &["--list"], "v1"),
        ("blame", &["-L", "2,2", "--", "tracked.txt"], "second"),
        ("config", &["--get", "core.filemode"], "true"),
    ];
    for &(subcommand, args, expected) in cases {
        let value = run(&scope, subcommand, args)
            .await
            .with_context(|| format!("{subcommand} {args:?}"))?;
        assert_eq!(value["exit_code"], 0, "{value}");
        assert!(
            value["text"].as_str().unwrap().contains(expected),
            "{subcommand} {value}"
        );
    }
    fs::write(f.repo.join("large"), vec![b'x'; MAX_BYTES * 3])?;
    let id = git(&f.repo, &["hash-object", "-w", "large"])?;
    let blob = run(&scope, "cat-file", &["-p", &id]).await?;
    assert_eq!(blob["truncated"], true);
    assert_eq!(blob["text"].as_str().unwrap().len(), MAX_BYTES);
    fs::write(f.repo.join("tracked.txt"), vec![b'x'; MAX_BYTES * 3])?;
    let stat = run(&scope, "diff", &["--stat", "--", "tracked.txt"]).await?;
    assert!(stat["text"].as_str().unwrap().contains("1 file changed"));
    assert_eq!(stat["truncated"], false);
    let diff = run(&scope, "diff", &[]).await?;
    assert_eq!(diff["truncated"], true);
    assert_eq!(diff["text"].as_str().unwrap().len(), MAX_BYTES);
    Ok(())
}

#[tokio::test]
async fn repository_settings_cannot_run_helpers_or_write_the_index() -> Result<()> {
    let f = Fixture::new()?;
    let scope = f.scope()?;
    let marker = f.repo.join("helper-ran");
    let script = f.repo.join("helper");
    fs::write(&script, "#!/bin/sh\ntouch helper-ran\ncat\n")?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    let script = script.to_str().unwrap();
    git(&f.repo, &["config", "core.fsmonitor", script])?;
    git(&f.repo, &["config", "core.pager", script])?;
    git(&f.repo, &["config", "diff.external", script])?;
    git(&f.repo, &["config", "diff.custom.textconv", script])?;
    git(&f.repo, &["config", "filter.custom.clean", script])?;
    git(&f.repo, &["config", "filter.custom.required", "true"])?;
    git(
        &f.repo,
        &["config", "include.path", "/nonexistent/git-read-config"],
    )?;
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.url",
            "https://user:password@example.invalid/path-token?query-token#fragment-token",
        ],
    )?;
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.pushurl",
            "helper::private-argument",
        ],
    )?;
    git(
        &f.repo,
        &["config", "core.worktree", "/nonexistent/git-read-worktree"],
    )?;
    fs::write(
        f.repo.join(".gitattributes"),
        "tracked.txt filter=custom diff=custom\n",
    )?;
    fs::write(f.repo.join("tracked.txt"), "changed\n")?;
    let index = f.repo.join(".git/index");
    let before = (fs::read(&index)?, index.metadata()?.modified()?);
    let lock = f.repo.join(".git/index.lock");
    assert!(!lock.exists());
    assert!(
        run(&scope, "status", &[]).await?["text"]
            .as_str()
            .unwrap()
            .contains(" M tracked.txt")
    );
    assert!(!lock.exists());
    fs::write(&lock, "existing lock")?;
    assert!(
        run(&scope, "diff", &[]).await?["text"]
            .as_str()
            .unwrap()
            .contains("+changed")
    );
    run(&scope, "blame", &["-L", "1,1", "--", "tracked.txt"]).await?;
    assert_eq!(fs::read_to_string(lock)?, "existing lock");
    assert_eq!((fs::read(&index)?, index.metadata()?.modified()?), before);
    assert!(!marker.exists());

    let remote = run(&scope, "remote", &["-v"]).await?;
    assert_eq!(
        remote["text"],
        "origin\thttps://example.invalid (fetch)\norigin\t[redacted] (push)\n"
    );
    Ok(())
}

#[tokio::test]
async fn assigned_linked_worktree_and_independent_clone_use_their_own_head() -> Result<()> {
    let f = Fixture::new()?;
    for name in ["linked", "clone"] {
        let cwd = f.temp.path().join(name);
        if name == "linked" {
            git(
                &f.repo,
                &["worktree", "add", "-b", name, cwd.to_str().unwrap()],
            )?;
        } else {
            git(
                f.temp.path(),
                &[
                    "clone",
                    "--no-local",
                    f.repo.to_str().unwrap(),
                    cwd.to_str().unwrap(),
                ],
            )?;
            git(&cwd, &["checkout", "-b", name])?;
        }
        fs::write(cwd.join("work.txt"), "only in the assigned checkout")?;
        let worker = crate::worker_scope::Scope::open(&f.repo, &cwd, f.temp.path())?;
        let scope = ReadScope::new(&f.repo, &f.temp.path().join("state"), Some(&worker))?;
        assert_eq!(
            run(&scope, "rev-parse", &["--abbrev-ref", "HEAD"]).await?["text"],
            format!("{name}\n")
        );
        assert_eq!(run(&scope, "status", &[]).await?["text"], "?? work.txt\n");
        assert_eq!(run(&scope, "ls-files", &[]).await?["text"], "tracked.txt\n");
    }
    Ok(())
}

#[test]
fn mcp_schema_and_per_tool_approvals() {
    use crate::mcp;

    let tools = mcp::partner_tool_list(false);
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == NAME)
        .unwrap();
    let schema = &tool["inputSchema"];
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert_eq!(schema["required"], json!(["subcommand"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["subcommand"]["enum"],
        json!(OPERATIONS)
    );
    assert_eq!(schema["properties"]["args"]["items"]["type"], "string");
    assert!(serde_json::from_value::<Args>(json!({"subcommand": "status", "cwd": "/"})).is_err());

    let worker = mcp::worker_tool_names(&[]);
    assert!(worker.contains(&NAME.into()));
    assert!(!mcp::worker_tool_names(&["fs_read".into()]).contains(&NAME.into()));
    assert_eq!(
        mcp::codex_tool_policy(Some(&worker))[NAME]["approval_mode"],
        "approve"
    );
    assert_eq!(
        mcp::codex_tool_policy(None)[NAME]["approval_mode"],
        "approve"
    );
    assert!(mcp::codex_tool_policy(Some(&[])).get(NAME).is_none());
    assert!(mcp::claude_tool_names(false, true).any(|name| name == "mcp__fuzukue__git_read"));
}
