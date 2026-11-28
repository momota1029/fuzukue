use super::*;
use serde_json::json;

fn state() -> Vec<PathBuf> {
    vec![PathBuf::from("/srv/state")]
}

#[test]
fn external_destructive_administrator_and_credential_commands_need_a_person() {
    use Reason::*;
    for (text, expected) in [
        ("git push", ExternalWrite),
        ("git -C sub push origin main", ExternalWrite),
        ("cargo test && git push --force", ExternalWrite),
        ("/bin/bash -lc 'git push'", ExternalWrite),
        ("/bin/bash -lc \"cd x && git push\"", ExternalWrite),
        ("env GIT_TRACE=1 timeout 30 git push", ExternalWrite),
        ("find . -exec git push ;", ExternalWrite),
        ("find . -exec git push \\;", ExternalWrite),
        ("echo $(git push)", ExternalWrite),
        ("echo \"done: `git push`\"", ExternalWrite),
        ("gh pr create --title x", ExternalWrite),
        ("gh release upload v1 file", ExternalWrite),
        ("gh api -X POST repos/a/b/issues", ExternalWrite),
        ("gh api repos/a/b/issues -f title=x", ExternalWrite),
        ("curl -X POST https://example.test/api", ExternalWrite),
        ("curl -sd x=1 https://example.test", ExternalWrite),
        ("curl --json '{}' https://localhost.example.test", ExternalWrite),
        ("cargo publish", ExternalWrite),
        ("cargo +nightly --config a=b publish", ExternalWrite),
        ("npm run deploy", ExternalWrite),
        ("npx vercel deploy", ExternalWrite),
        ("scp file host:/tmp", ExternalWrite),
        ("git reset --hard HEAD~1", DestructiveGit),
        ("git reset --har", DestructiveGit),
        ("git clean -fdx", DestructiveGit),
        ("git checkout -- src/main.rs", DestructiveGit),
        ("git restore .", DestructiveGit),
        ("git branch -D topic", DestructiveGit),
        ("git rebase main", DestructiveGit),
        ("git commit --amend --no-edit", DestructiveGit),
        ("git stash drop", DestructiveGit),
        ("rm -rf .git", DestructiveGit),
        ("rm -rf ~", DestructiveGit),
        ("rm -rf \"$HOME\"", DestructiveGit),
        ("sudo apt-get install jq", Administrator),
        ("apt install jq", Administrator),
        ("cat ~/.ssh/id_ed25519", Credentials),
        ("cat \"$HOME/.ssh/id_ed25519\"", Credentials),
        ("cat /srv/state/server-token", Credentials),
        ("cat .fuzukue/memory/index.jsonl", Credentials),
        ("cat < /srv/state/server-token", Credentials),
        ("cat /srv/state/worker-abc/.claude/.credentials.json", Credentials),
        ("gh auth token", Credentials),
        ("git credential fill", Credentials),
    ] {
        assert_eq!(command(text, &state()), Some(expected), "{text:?}");
    }
}

#[test]
fn publish_flags_and_sweeping_deletes_need_a_person() {
    use Reason::*;
    for text in [
        "docker buildx build --push -t example.test/app .",
        "docker build --push=true .",
        "podman build --push=false .",
        "docker buildx bake --push",
        "docker buildx build --output type=registry .",
        "docker buildx build -o type=image,name=app,push=true .",
        "docker buildx build --cache-to=type=registry,ref=example.test/cache .",
        "docker compose publish example.test/app",
        "docker buildx imagetools create -t example.test/app:v1 example.test/app:v0",
    ] {
        assert_eq!(command(text, &state()), Some(ExternalWrite), "{text:?}");
    }
    for text in [
        "rm -rf ./*",
        "rm -rf ../",
        "rm -rf ..",
        "rm -rf ../*",
        "rm -rf ../..",
        "rm -r .//*",
        "rm -rf .*",
        "rm --recursive -- ./",
        "rm -rf \"$OUT\"/*",
        "rm -rf \"$OUT/\"",
        "rm -rf ${OUT}/build",
        "rm -rf $prefix*",
        "rm -rf \"$(pwd)\"/*",
        "rm -rf ./\"$OUT\"/*",
        "rm -rf ~/\"$OUT\"",
        "rm -rf \"$HOME\"/./*",
    ] {
        assert_eq!(command(text, &state()), Some(DestructiveGit), "{text:?}");
    }
    for text in [
        "docker build -t app .",
        "docker buildx build --load .",
        "rm -rf ./target",
        "rm -rf ../scratch/out",
        "rm -rf target/*",
        "rm -rf \"target/$name\"",
        "rm -rf \"$dir\"",
        "rm -f \"$OUT\"/*",
    ] {
        assert_eq!(command(text, &state()), None, "{text:?}");
    }
}

/// Each group once slipped past the check, or could.
#[test]
fn comments_here_documents_wrappers_and_redirects_do_not_hide_a_command() {
    use Reason::*;
    for (text, expected) in [
        // A quote in a comment or a here-document body disabled the rest.
        ("# don't stop\ngit push", ExternalWrite),
        ("echo ok # it's fine\ngit push", ExternalWrite),
        ("cat <<'EOF'\nit's data\nEOF\ngit push", ExternalWrite),
        ("cat <<EOF > notes\nsay \"hi\nEOF\ngit push", ExternalWrite),
        ("cat <<EOF\n$(git push)\nEOF", ExternalWrite),
        ("cat <<EOF && git push\nx\nEOF", ExternalWrite),
        ("cat <<EOF | bash\ngit status\nEOF", Unclassified),
        // Shells.
        ("bash -cx 'git push'", ExternalWrite),
        ("bash -xc 'git push'", ExternalWrite),
        ("bash -c -- 'git push'", ExternalWrite),
        ("bash <<< 'git push'", ExternalWrite),
        ("bash -s <<'EOF'\ngit push\nEOF", ExternalWrite),
        ("sh <<EOF\ngit push\nEOF", ExternalWrite),
        ("echo git push | sh", Unclassified),
        ("bash < steps.sh", Unclassified),
        ("bash -c \"$cmd\"", Unclassified),
        ("eval \"$cmd\"", Unclassified),
        ("eval 'git push'", ExternalWrite),
        ("trap 'git push' EXIT", ExternalWrite),
        // Wrappers and their options.
        ("env -u X git push", ExternalWrite),
        ("env --unset=X -i git push", ExternalWrite),
        ("env -S 'git push'", Unclassified),
        ("timeout -s KILL 5 git push", ExternalWrite),
        ("timeout --signal KILL -k 1 5 git push", ExternalWrite),
        ("nice -n 5 git push", ExternalWrite),
        ("stdbuf -o L git push", ExternalWrite),
        ("flock f git push", ExternalWrite),
        ("flock -w 5 /tmp/lock git push", ExternalWrite),
        ("flock f -c 'git push'", ExternalWrite),
        ("nohup setsid git push", ExternalWrite),
        ("time git push", ExternalWrite),
        ("echo origin | xargs git push", ExternalWrite),
        ("echo push | xargs git", Unclassified),
        ("watch git push", Unclassified),
        ("strace -f git push", Unclassified),
        // Redirects before and between the words.
        (">out git push", ExternalWrite),
        ("2>/dev/null git push", ExternalWrite),
        ("> out 2>&1 git push", ExternalWrite),
        ("git > out push", ExternalWrite),
        ("git 2>&1 push", ExternalWrite),
        // Checkout, switch and pull.
        ("git checkout src/main.rs", DestructiveGit),
        ("git checkout Makefile", DestructiveGit),
        ("git checkout HEAD~1 src/main.rs", DestructiveGit),
        ("git checkout -B topic", DestructiveGit),
        ("git checkout -b topic main src/main.rs", DestructiveGit),
        ("git worktree add -B topic dir", DestructiveGit),
        ("git pull --rebase", DestructiveGit),
        ("git pull -r", DestructiveGit),
        ("git pull --rebase=merges origin main", DestructiveGit),
        ("git -c pull.rebase=true pull", DestructiveGit),
        // URLs without a scheme beside a local one.
        ("curl -X POST http://127.0.0.1:8080/ example.com", ExternalWrite),
        ("curl -d x=1 localhost:1 --url example.com", ExternalWrite),
        ("curl -d x=1 -x http://proxy.test http://localhost/", ExternalWrite),
        ("http_proxy=http://proxy.test curl -d x=1 http://localhost/", ExternalWrite),
        ("curl -d @file example.com/upload", ExternalWrite),
        // Quoting.
        ("g\\it pu''sh", ExternalWrite),
        ("\"git\" 'push'", ExternalWrite),
        // Names and options only the shell knows.
        ("$GIT push", Unclassified),
        ("\"$(which git)\" push", Unclassified),
        ("`which git` push", Unclassified),
        ("g?t push", Unclassified),
        ("git $verb", Unclassified),
        ("git pu{sh,}", Unclassified),
        ("git reset $mode", Unclassified),
        ("git -c alias.p=push p", Unclassified),
        ("git publish-it", Unclassified),
        ("cargo $verb", Unclassified),
        ("gh pr $verb", Unclassified),
        ("curl $options https://example.test", Unclassified),
        ("curl -K options.txt", Unclassified),
        ("alias g='git push'", Unclassified),
        ("git pu\\\nsh", Unclassified),
        ("coproc git push", Unclassified),
        // Compound statements and the rest of a here-document's first line.
        ("{ git push; }", ExternalWrite),
        ("(git push)", ExternalWrite),
        ("f() { git push; }; f", ExternalWrite),
        ("case $x in a) git push;; esac", ExternalWrite),
        ("for r in a b; do git push \"$r\"; done", ExternalWrite),
        ("if ! git push; then :; fi", ExternalWrite),
        ("diff <(git push) b", ExternalWrite),
        ("x=$(git push)", ExternalWrite),
        ("export X=\"$(git push)\"", ExternalWrite),
        ("[[ -n $(git push) ]]", ExternalWrite),
        ("echo \"${x:-$(git push)}\"", ExternalWrite),
        ("echo a#b $# ${#x}; git push", ExternalWrite),
        ("cat <<EOF; git push\nx\nEOF", Unclassified),
        ("cat <<-EOF\n\tx\n\tEOF\ngit push", ExternalWrite),
        ("cat <<A <<B\na'\nA\nb\"\nB\ngit push", Unclassified),
        // Lines that cannot be parsed.
        ("echo 'open", Unclassified),
        ("if true; then", Unclassified),
        ("echo $(", Unclassified),
    ] {
        assert_eq!(command(text, &state()), Some(expected), "{text:?}");
    }
}

#[test]
fn ordinary_local_work_runs_without_asking() {
    for text in [
        "cargo test -q -p secretaire",
        "mkdir -p target/x && cp a b && rm -rf target/x",
        "git status && git diff | head",
        "git add -A && git commit -m 'Describe the push button'",
        "git commit -m \"$(cat <<'EOF'\nDescribe the push button\n\nIt's only data: git push\nEOF\n)\"",
        "git commit -m \"$message\"",
        "git log --grep 'git push'",
        "git log --oneline \"$base..HEAD\"",
        "rg -n 'sudo' README.md",
        "# git push later\ncargo build",
        "cat > notes.md <<'EOF'\ngit push\nEOF",
        "git switch -c topic",
        "git checkout -b topic",
        "git checkout -b \"$name\" main",
        "git checkout --detach HEAD~1",
        "git checkout -",
        "git switch main",
        "git branch -d merged",
        "git reset HEAD file",
        "git restore --staged file",
        "git fetch && git pull --ff-only",
        "git -C \"$dir\" status",
        "gh pr view 3 && gh issue list && gh api repos/a/b",
        "gh pr view \"$number\"",
        "curl -s https://example.test/page",
        "curl -X POST http://127.0.0.1:8080/test -d x=1",
        "curl -s -H 'a: b' -d x=1 \"http://localhost:$port/test\"",
        "npm install && npm test",
        "pip install -r requirements.txt",
        "cat /srv/state/worker-abc/output.txt",
        "for f in src/*.rs; do wc -l \"$f\"; done",
        "if [ -f Cargo.toml ]; then cargo build 2>&1 | tail -n 5; fi",
        "CARGO_TARGET_DIR=\"$PWD/target\" timeout 600 cargo test -- --nocapture",
        "find . -name '*.rs' -exec grep -l TODO {} +",
        "ls | xargs -I{} wc -l {}",
        "bash scripts/worker-cargo.sh test",
        "cargo test \\\n  -p secretaire",
        "(cd sub && make test) > log 2>&1 &",
        "command -v git",
        // Search patterns and compound reads are data, not commands.
        "cd /work/crates && rg -n \"enum Mode|sudo|git push\" -B6 -A16 src/lib.rs; rg -n 'fn a|ssh' -A14 src/main.rs",
    ] {
        assert_eq!(command(text, &state()), None, "{text:?}");
    }
}

#[test]
fn plain_reads_are_told_to_use_the_file_tools_a_bounded_number_of_times() {
    for text in [
        "ls", "cat a b", "rg -n 'a|b' src", "grep -rn x . | head -20", "sed -n '1,40p' a", "sed -n '10,$p' a",
        "find . -name '*.rs'", "tail -n 3 a", "cd /w && rg -n \"fn (a|b)\" -A14 src/lib.rs; rg -n x src",
    ] {
        assert!(plain_read(&json!({"command": text, "description": "read"})), "{text}");
    }
    for text in [
        "", "cd /w", "pwd", "cat a > b", "cat a 2>/dev/null", "cat $(ls)", "cat \"`ls`\"", "rg x $HOME",
        "find . -delete", "find . -exec cat {} ;", "rg --pre sh x", "sed -i s/a/b/ f", "sed -n '1e id' f",
        "tail -f log", "ls & rm x", "ls; rm x", "cat a | sh", "ls || rm x && curl x", "python3 -c 1",
        "curl https://example.test", "FOO=1 ls", "(ls)", "cat <<EOF", "cat 'a", "ls # note",
    ] {
        assert!(!plain_read(&json!({"command": text})), "{text}");
    }
    assert!(!plain_read(&json!({"command": "ls", "run_in_background": true})));

    let mut refusals = Refusals::default();
    let input = json!({"command": "rg x"});
    assert!(refusals.refuse(&input, None).is_some_and(|message| message.contains("fs_search")));
    assert!(refusals.refuse(&input, None).is_some());
    assert!(refusals.refuse(&input, None).is_none());
    assert!(refusals.refuse(&json!({"command": "cat key"}), Some(Reason::Credentials)).is_none());
    assert_eq!((0..40).filter(|n| refusals.refuse(&json!({"command": format!("ls {n}")}), None).is_some()).count(),
        REFUSALS_PER_RUN - 2);
}

#[test]
fn tool_requests_and_permission_grants_are_checked() {
    let state = state();
    assert_eq!(request("Read", &json!({"file_path": "/srv/state/settings.json"}), &state), Some(Reason::Credentials));
    assert_eq!(request("Glob", &json!({"pattern": "/srv/state/*"}), &state), Some(Reason::Credentials));
    assert_eq!(request("Write", &json!({"file_path": "notes.md", "content": "git push"}), &state), None);
    assert_eq!(request("Monitor", &json!({"command": "tail -f log"}), &state), None);
    assert_eq!(request("Monitor", &json!({"command": "while true; do git push; done"}), &state),
        Some(Reason::ExternalWrite));
    assert_eq!(request("Monitor", &json!({"ws": {"url": "wss://example.test"}}), &state), Some(Reason::Unclassified));
    assert_eq!(request("item/commandExecution/requestApproval", &json!({"command": ["bash", "-lc", "git push"]}), &state),
        Some(Reason::ExternalWrite));
    assert_eq!(request("item/commandExecution/requestApproval", &json!({}), &state), Some(Reason::Unclassified));
    assert_eq!(request("item/permissions/requestApproval",
        &json!({"permissions": {"network": {"enabled": true}}}), &state), Some(Reason::ExternalWrite));
    assert_eq!(request("item/permissions/requestApproval",
        &json!({"permissions": {"fileSystem": {"write": ["/srv/state"]}}}), &state), Some(Reason::Credentials));
    assert_eq!(request("item/permissions/requestApproval",
        &json!({"permissions": {"fileSystem": {"write": ["/work/project"]}}}), &state), None);
}

#[test]
fn only_listed_native_tools_can_be_granted_automatically() {
    for tool in ["Bash", "Read", "Edit", "Write", "Monitor", "TodoWrite", "Agent", "mcp__fuzukue__fs_read"] {
        assert!(automatic_tool(tool), "{tool}");
    }
    for tool in ["WebFetch", "WebSearch", "SendMessage", "RemoteTrigger", "CronCreate", "PushNotification",
        "Workflow", "ExitPlanMode", "mcp__outside__send", "SomethingNew"]
    {
        assert!(!automatic_tool(tool), "{tool}");
    }
}

#[cfg(unix)]
#[test]
fn patch_paths_are_checked_where_they_really_point() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let (project, state) = (root.join("project"), root.join("state"));
    std::fs::create_dir_all(project.join(".git"))?;
    std::fs::create_dir(&state)?;
    std::os::unix::fs::symlink(&state, project.join("escape"))?;
    let check = |changes: serde_json::Value| request("item/fileChange/requestApproval",
        &json!({"cwd": project, "changes": changes}), std::slice::from_ref(&state));
    let one = |path: &str| check(json!([{"path": path}]));
    assert_eq!(one("src/main.rs"), None);
    assert_eq!(one("escape/settings.json"), Some(Reason::Credentials));
    assert_eq!(one(state.join("settings.json").to_str().unwrap()), Some(Reason::Credentials));
    assert_eq!(one("/home/someone/.ssh/config"), Some(Reason::Credentials));
    assert_eq!(one(".git/hooks/pre-commit"), Some(Reason::DestructiveGit));
    assert_eq!(check(json!([{"path": "a", "kind": {"type": "update", "move_path": "escape/a"}}])),
        Some(Reason::Credentials));
    assert_eq!(check(json!([])), Some(Reason::Unclassified));
    assert_eq!(request("item/fileChange/requestApproval", &json!({"cwd": project}), &[]), Some(Reason::Unclassified));
    Ok(())
}
