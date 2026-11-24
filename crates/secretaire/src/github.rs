//! GitHub を作業場にする（cards/design/issue-queue）。
//!
//! 開いている Issue と `work/issue-<N>` ブランチを `gh` で定期的に読み、クライアントに配る。
//! Issue をローカルの Claude に任せるときの依頼文もここで組み立てる。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fuzukue_proto::{BranchView, IssueView, ServerEvent};
use serde_json::Value;
use tokio::process::Command;

use crate::{issue_threads, Workspace};

pub(crate) async fn gh(project: &Path, args: &[&str]) -> Result<String> {
    gh_command(Command::new("gh"), project, args).await
}

pub(crate) async fn output(command: Command, project: &Path, args: &[&str]) -> Result<std::process::Output> {
    let limit = fuzukue_proto::compat::env("FUZUKUE_GH_TIMEOUT_SECONDS").ok()
        .and_then(|value| value.parse::<u64>().ok()).filter(|seconds| *seconds > 0).unwrap_or(120);
    output_with_timeout(command, project, args, Duration::from_secs(limit)).await
}

async fn output_with_timeout(
    mut command: Command, project: &Path, args: &[&str], limit: Duration,
) -> Result<std::process::Output> {
    if crate::canary() {
        bail!(harness_text!("GitHub actions are unavailable in the canary.", "試運転からは GitHub を操作できないよ。"));
    }
    command.args(args).current_dir(project);
    tokio::time::timeout(limit, crate::cancellation::command_output(command)).await
        .context("GitHub CLI (gh) timed out")?.context("gh を実行できない")
}

pub(crate) async fn gh_command(command: Command, project: &Path, args: &[&str]) -> Result<String> {
    let out = output(command, project, args).await?;
    if !out.status.success() {
        bail!("gh {}: {}", args.first().copied().unwrap_or_default(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 本文から入口カードを拾う。`cards: a, b` / `cards: [a, b]` の行と、本文中の `[[id]]`。
pub fn entry_cards(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().trim_matches(|c| matches!(c, '`' | '[' | ']' | '"' | '\'')).trim();
        if !s.is_empty() && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    for line in body.lines() {
        let l = line.trim().trim_start_matches(['-', '*', ' ']);
        if let Some(rest) = l.strip_prefix("cards:").or_else(|| l.strip_prefix("入口カード:")).or_else(|| l.strip_prefix("入口カード：")) {
            for part in rest.split([',', '、', ' ']) {
                push(part);
            }
        }
    }
    // コードブロックとインラインコードの中は例として書かれていることが多いので読まない。
    let mut prose = String::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            prose.extend(line.split('`').step_by(2));
            prose.push('\n');
        }
    }
    let mut rest = prose.as_str();
    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start..].find("]]") else { break };
        let inner = &rest[start + 2..start + end];
        let target = inner.split('|').next().unwrap_or(inner);
        if !target.starts_with("file:") {
            push(target);
        }
        rest = &rest[start + end + 2..];
    }
    out
}

pub(crate) async fn open_issue_rows(app: &Workspace, repo: &str) -> Result<Vec<Value>> {
    issue_threads::rest::pages(
        app,
        &format!("repos/{repo}/issues?state=open&sort=created&direction=desc&per_page=100"),
        None,
    )
    .await
}

pub async fn open_issues(app: &Workspace, repo: &str, list: &[Value]) -> Result<Vec<IssueView>> {
    use issue_threads::rest;
    let branches = rest::pages(app, &format!("repos/{repo}/branches?per_page=100"), None).await?;
    let mut issues = Vec::new();
    for v in list {
        if v["pull_request"].is_object() || crate::workshop::is_research_issue(v) {
            continue;
        }
        let Some(number) = v["number"].as_u64() else { continue };
        let body = v["body"].as_str().unwrap_or("").to_string();
        let branch_name = format!("work/issue-{number}");
        let branch = if let Some(branch) = branches.iter().find(|branch| branch["name"] == branch_name) {
            if let Some(sha) = branch["commit"]["sha"].as_str().filter(|sha| sha.bytes().all(|byte| byte.is_ascii_hexdigit())) {
                match rest::get(app, &format!("repos/{repo}/commits/{sha}")).await {
                    Ok(value) => Some(BranchView {
                        name: branch_name,
                        last_commit_at: value["commit"]["committer"]["date"].as_str().unwrap_or_default().into(),
                        last_message: value["commit"]["message"].as_str().and_then(|msg| msg.lines().next()).unwrap_or_default().into(),
                    }),
                    Err(error) if issue_threads::rate_limited(&error) => return Err(error),
                    Err(error) => {
                        tracing::warn!("GitHub issue branch: {error:#}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        issues.push(IssueView {
            number,
            title: v["title"].as_str().unwrap_or("").into(),
            labels: v["labels"].as_array().into_iter().flatten().filter_map(|l| l["name"].as_str().map(String::from)).collect(),
            cards: entry_cards(&body),
            body,
            url: v["html_url"].as_str().unwrap_or("").into(),
            updated_at: v["updated_at"].as_str().unwrap_or("").into(),
            branch,
            comments: v["comments"].as_u64().unwrap_or(0) as usize,
        });
    }
    issues.sort_by_key(|issue| issue.number);
    Ok(issues)
}

/// 最後に読んだ一覧（あとから接続したクライアントに送る）。
pub type LastIssues = Arc<std::sync::Mutex<Option<ServerEvent>>>;

pub(crate) fn worktree_path(project: &Path, number: u64) -> Result<std::path::PathBuf> {
    let name = project.file_name().context("project has no name")?.to_string_lossy().to_string();
    Ok(project.parent().context("project has no parent")?.join(format!("{name}-issue-{number}")))
}

/// Issue 用の作業ディレクトリ（git worktree）を用意する。`<project>-issue-<N>` に、
/// ブランチ `work/issue-<N>` で作る。リモートにそのブランチがあればそこから、なければ origin/main から。
pub async fn ensure_worktree(project: &Path, number: u64) -> Result<std::path::PathBuf> {
    let dir = worktree_path(project, number)?;
    if dir.exists() {
        return Ok(dir);
    }
    let git = |args: Vec<String>| async move {
        let out = Command::new("git").args(&args).current_dir(project).output().await?;
        anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        anyhow::Ok(())
    };
    let branch = format!("work/issue-{number}");
    git(vec!["fetch".into(), "-q".into(), "origin".into()]).await?;
    let remote_has = Command::new("git")
        .args(["rev-parse", "--verify", "-q", &format!("origin/{branch}")])
        .current_dir(project)
        .status()
        .await?
        .success();
    let base = if remote_has { format!("origin/{branch}") } else { "origin/main".into() };
    git(vec!["worktree".into(), "add".into(), "-B".into(), branch, dir.to_string_lossy().into(), base]).await?;
    Ok(dir)
}

/// ローカルの Claude に Issue を任せるときの依頼文。
pub async fn work_prompt(project: &Path, repo: &str, number: u64) -> Result<String> {
    let raw = gh(project, &["issue", "view", &number.to_string(), "--repo", repo, "--json", "number,title,body,url"]).await?;
    let v: Value = serde_json::from_str(&raw)?;
    anyhow::ensure!(!crate::workshop::is_research_issue(&v), "研究 Issue は研究の一覧から開始する");
    let title = v["title"].as_str().unwrap_or("");
    let body = v["body"].as_str().unwrap_or("");
    let entries = entry_cards(body);
    let reading = match fuzukue_cards::Graph::open(project) {
        Ok(g) if !entries.is_empty() => g
            .context(&entries)
            .iter()
            .map(|id| harness_format!("- `{}` ({}) {}", "- `{}`（{}）{}", id, g.cards[id].path.display(), g.cards[id].title))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => harness_text!("(No entry cards specified)", "（入口カードの指定なし）").into(),
    };
    Ok(harness_format!(
        "Work on GitHub workshop issue #{number} “{title}”.\n\n## Issue body\n\n{body}\n\n## Cards to read first (dependencies first)\n\n{reading}\n\n## Workflow (docs/workshop.md)\n\nThis directory is the issue’s Git worktree on `work/issue-{number}`. Stay on that branch.\n\n1. Mark work as started with `gh issue edit {number} --add-label dot:running --remove-label dot:ready`.\n2. Check that the branch is `work/issue-{number}` with `git status`.\n3. Do the work. Update links among changed files and cards, and pass `fuzukue-cards check` and relevant tests.\n4. Commit at checkpoints and run `git push -u origin work/issue-{number}`.\n5. When done, run `gh issue comment {number}` to report what changed, what you verified, what remains unverified and what is left. Include `<!-- fuzukue -->` in every comment this turn. Then run `gh issue edit {number} --add-label dot:review --remove-label dot:running`.\n\nLeave the merge to main to the supervisor, who will verify and accept the work. If acceptance criteria are unclear, ask in an issue comment and pause before starting.", "GitHub の作業場の Issue #{number}「{title}」に取り組む。\n\n\
         ## Issue の本文\n\n{body}\n\n\
         ## 先に読むカード（依存が先の順）\n\n{reading}\n\n\
         ## 進め方（docs/workshop.md）\n\n\
         いまの作業ディレクトリは、この Issue 用の git worktree（ブランチ `work/issue-{number}`）。ブランチを切り替えない。\n\n\
         1. `gh issue edit {number} --add-label dot:running --remove-label dot:ready` で取りかかったことを示す。\n\
         2. ブランチが `work/issue-{number}` であることを `git status` で確かめる。\n\
         3. 作業する。触ったファイルとカードのリンクを更新し、`fuzukue-cards check` と関係するテストを通す。\n\
         4. 区切りごとにコミットし、`git push -u origin work/issue-{number}`。\n\
         5. 終わったら `gh issue comment {number}` に、やったこと・確かめたこと・確かめていないこと・残りを書き、このターンの各コメントには `<!-- fuzukue -->` を入れる。\n\
            `gh issue edit {number} --add-label dot:review --remove-label dot:running`。\n\n\
         main へのマージはしない（監督役が検証して取り込む）。受け入れ条件が曖昧なら、作業の前に Issue にコメントで質問して止まる。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn github_timeout_cancels_the_command_and_its_helpers() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        let mut command = Command::new("sh");
        command.arg("-c").arg(r#"
            (touch entered; while [ ! -f release ]; do sleep 0.02; done; touch wrote) &
            wait
        "#);
        let result = output_with_timeout(command, dir.path(), &[], Duration::from_millis(500)).await;
        assert!(result.unwrap_err().to_string().contains("gh) timed out"));
        assert!(dir.path().join("entered").exists());
        dir.write("release", "");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!dir.path().join("wrote").exists());
        Ok(())
    }

    #[test]
    fn picks_cards_line_and_links() {
        let body = "やること\n\ncards: agent-co-op, `protocol`\n本文で [[usage-awareness]] と [[file:src/x.rs]] に触れる。";
        assert_eq!(entry_cards(body), vec!["agent-co-op", "protocol", "usage-awareness"]);
    }

    #[test]
    fn ignores_links_in_code() {
        let body = "例: `[[id|表示]]`\n```\n[[x]]\n```\n本物 [[real]]";
        assert_eq!(entry_cards(body), vec!["real"]);
    }
}
