use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use tokio::process::Command;

use super::{Entry, Kind};

async fn git(project: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.args(args).current_dir(project).env("GIT_TERMINAL_PROMPT", "0");
    let output = crate::cancellation::command_output(command).await?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

async fn git_line(project: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(project, args).await?)?.trim().into())
}

pub(super) async fn origin(root: &Path) -> Option<String> {
    let url = git_line(root, &["remote", "get-url", "origin"])
        .await
        .ok()?;
    super::repo_from_url(&url)
}

pub(super) async fn worktree(
    project: &Path,
    number: u64,
    base: Option<&str>,
    remote: bool,
) -> Result<(PathBuf, String)> {
    let name = project
        .file_name()
        .context("研究フォルダの名前がない")?
        .to_string_lossy();
    let dir = project.with_file_name(format!("{name}-issue-{number}"));
    let branch = format!("work/issue-{number}");
    let branch_ref = format!("refs/heads/{branch}");
    ensure!(
        git_line(project, &["rev-parse", "--show-toplevel"]).await? == project.to_string_lossy(),
        "研究ワークスペースのルートに Git リポジトリが必要"
    );
    let main = git_line(project, &["rev-parse", "--verify", "refs/heads/main"]).await?;
    let upstream = format!("refs/remotes/origin/{branch}");
    if remote {
        git(
            project,
            &["fetch", "origin", &format!("{branch_ref}:{upstream}")],
        )
        .await?;
    }
    if !dir.exists() {
        if git_line(project, &["rev-parse", "--verify", &branch_ref])
            .await
            .is_ok()
        {
            git(
                project,
                &[
                    "worktree",
                    "add",
                    dir.to_str().context("パスが不正")?,
                    &branch,
                ],
            )
            .await?;
        } else {
            git(
                project,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    dir.to_str().context("パスが不正")?,
                    if remote { &upstream } else { &main },
                ],
            )
            .await?;
        }
    }
    ensure!(
        !dir.symlink_metadata()?.file_type().is_symlink(),
        "作業ディレクトリがリンクになっている"
    );
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    ensure!(
        git_line(&dir, &args).await? == git_line(project, &args).await?
            && git_line(&dir, &["symbolic-ref", "HEAD"]).await? == branch_ref
            && git_line(&dir, &["rev-parse", "--show-toplevel"]).await? == dir.to_string_lossy(),
        "研究 Issue の worktree またはブランチが一致しない"
    );
    if remote {
        ensure!(
            git_line(&dir, &["status", "--porcelain"]).await?.is_empty(),
            "dots の作業ツリーに未保存の変更がある"
        );
        git(&dir, &["merge", "--ff-only", &upstream]).await?;
        ensure!(
            git_line(&dir, &["rev-parse", "HEAD"]).await?
                == git_line(&dir, &["rev-parse", &upstream]).await?,
            "dots の作業ブランチとリモートが一致しない"
        );
    }
    let base = match base {
        Some(base) => base.into(),
        None => git_line(&dir, &["merge-base", &main, "HEAD"]).await?,
    };
    Ok((dir, base))
}

fn current(path: &Path) -> Result<Option<Vec<u8>>> {
    match path.symlink_metadata() {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "通常のファイルを指定する: {}",
                path.display()
            );
            Ok(Some(std::fs::read(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn checked_file(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        path.components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "相対パスが不正: {relative}"
    );
    let mut full = root.to_path_buf();
    for part in path.components() {
        full.push(part);
        match full.symlink_metadata() {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "リンクは取り込めない: {}",
                full.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(full)
}

pub(super) fn link(path: &Path, key: &str) -> Result<()> {
    let before = std::fs::read_to_string(path)?;
    let after = with_link(&before, key)?;
    Change {
        path: path.into(),
        before: Some(before.into_bytes()),
        after: after.into_bytes(),
    }
    .apply()
}

fn with_link(before: &str, key: &str) -> Result<String> {
    let (front, body) =
        fuzukue_cards::split_frontmatter(before).context("カードに front matter がない")?;
    let mut data: serde_yaml::Mapping = serde_yaml::from_str(front)?;
    let issues = data
        .entry("issues".into())
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
    let items = issues
        .as_sequence_mut()
        .context("カードの issues は配列で指定する")?;
    let key = serde_yaml::Value::String(key.into());
    if items.contains(&key) {
        return Ok(before.into());
    }
    items.push(key);
    let newline = if before.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    // Replace only the issues field, keeping other fields and the body byte-for-byte.
    let value = serde_yaml::to_string(&serde_yaml::Mapping::from_iter([(
        "issues".into(),
        issues.clone(),
    )]))?;
    let value = value.trim_end().replace('\n', newline);
    let mut lines = front.split_inclusive('\n').peekable();
    let mut edited = String::new();
    let mut found = false;
    while let Some(line) = lines.next() {
        if line.trim_end().starts_with("issues:")
            || line.starts_with("\"issues\":")
            || line.starts_with("'issues':")
        {
            found = true;
            edited.push_str(&value);
            edited.push_str(newline);
            while lines.peek().is_some_and(|line| {
                line.starts_with([' ', '\t', '-']) && !line.trim_start().starts_with('#')
            }) {
                lines.next();
            }
        } else {
            edited.push_str(line);
        }
    }
    if !found {
        if !edited.ends_with('\n') {
            edited.push_str(newline);
        }
        edited.push_str(&value);
    }
    if !edited.ends_with('\n') {
        edited.push_str(newline);
    }
    let after = format!("---{newline}{edited}---{newline}{body}");
    let parsed = fuzukue_cards::split_frontmatter(&after)
        .context("カードの front matter を更新できない")?
        .0;
    ensure!(
        serde_yaml::from_str::<serde_yaml::Mapping>(parsed)? == data,
        "カードの issues を安全に更新できない"
    );
    Ok(after)
}

fn note_path(dir: &Path, number: u64, index: usize) -> Result<PathBuf> {
    checked_file(
        dir,
        &format!("notes/workshop/issue-{number}/{}.md", index + 1),
    )
}

pub(super) fn read_summary(dir: &Path, number: u64) -> Result<String> {
    Ok(std::fs::read_to_string(checked_file(
        dir,
        &format!("notes/workshop/issue-{number}/summary.md"),
    )?)?)
}

fn append_note(card: &str, note: &str, marker: &str) -> Result<String> {
    if card.contains(marker) {
        return Ok(card.into());
    }
    let (_, body) = fuzukue_cards::split_frontmatter(card).context("カードに front matter がない")?;
    let newline = if card.starts_with("---\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut position = card.len() - body.len();
    let mut notes = false;
    for line in body.split_inclusive('\n') {
        let heading = line.trim_end() == "## Notes";
        if notes && line.starts_with("## ") {
            break;
        }
        position += line.len();
        notes |= heading;
    }
    let header = if notes {
        String::new()
    } else {
        format!("## Notes{newline}{newline}")
    };
    let note = note
        .trim()
        .lines()
        .map(|line| {
            if let Some(heading) = line.strip_prefix("## ").or_else(|| line.strip_prefix("# ")) {
                format!("### {heading}")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(newline);
    Ok(format!(
        "{}{newline}{newline}{header}{marker}{newline}{note}{newline}{newline}{}",
        card[..position].trim_end_matches(['\r', '\n']),
        &card[position..]
    ))
}

async fn lean_files(dir: &Path, base: &str) -> Result<BTreeSet<String>> {
    let tracked = git(
        dir,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            base,
            "--",
            "lean/",
        ],
    )
    .await?;
    let new = git(
        dir,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "lean/",
        ],
    )
    .await?;
    Ok(std::str::from_utf8(&tracked)?
        .split_terminator('\0')
        .chain(std::str::from_utf8(&new)?.split_terminator('\0'))
        .filter(|path| path.ends_with(".lean"))
        .map(str::to_owned)
        .collect())
}

fn read_note(dir: &Path, number: u64, index: usize) -> Result<Option<String>> {
    current(&note_path(dir, number, index)?)?
        .map(|note| {
            let note = String::from_utf8(note)?;
            ensure!(!note.trim().is_empty(), "下書き {}.md が空", index + 1);
            Ok(note)
        })
        .transpose()
}

pub(super) async fn has_output(
    dir: &Path,
    number: u64,
    base: &str,
    count: usize,
    kind: Kind,
) -> Result<()> {
    let mut notes = false;
    for index in 0..count {
        notes |= read_note(dir, number, index)?.is_some();
    }
    let files = if kind == Kind::Lean {
        lean_files(dir, base).await?
    } else {
        BTreeSet::new()
    };
    for path in &files {
        ensure!(
            current(&checked_file(dir, path)?)?.is_some(),
            "Lean ファイルの削除は手動で取り込む: {path}"
        );
    }
    ensure!(
        notes || !files.is_empty(),
        "取り込める下書きや Lean ファイルがない"
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}

impl Change {
    fn check(&self) -> Result<()> {
        let actual = current(&self.path)?;
        ensure!(
            actual == self.before || actual.as_ref() == Some(&self.after),
            "取り込み先が変わった: {}",
            self.path.display()
        );
        Ok(())
    }

    fn apply(&self) -> Result<()> {
        self.check()?;
        if current(&self.path)?.as_ref() == Some(&self.after) {
            return Ok(());
        }
        let parent = self.path.parent().context("保存先がない")?;
        std::fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        if let Ok(info) = self.path.metadata() {
            tmp.as_file().set_permissions(info.permissions())?;
        }
        tmp.write_all(&self.after)?;
        self.check()?;
        crate::durable::persist(tmp, &self.path)?;
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct Changes(Vec<Change>);

impl Changes {
    pub fn describe(&self) -> String {
        self.0
            .iter()
            .map(|change| change.path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn apply(&self) -> Result<()> {
        for change in &self.0 {
            change.check()?;
        }
        for change in &self.0 {
            change.apply()?;
        }
        Ok(())
    }
}

pub(super) async fn changes(
    project: &Path,
    dir: &Path,
    number: u64,
    key: &str,
    base: &str,
    entries: &[Entry],
    kind: Kind,
) -> Result<Changes> {
    let mut changes = Vec::new();
    let mut has_output = false;
    let marker = format!("<!-- labo-workshop-issue:{number} -->");
    for (index, entry) in entries.iter().enumerate() {
        let before = std::fs::read_to_string(&entry.path)?;
        let mut after = with_link(&before, key)?;
        let note = read_note(dir, number, index)?;
        if let Some(note) = &note {
            has_output = true;
            after = append_note(&after, note, &marker)?;
        }
        if after != before || note.is_some() {
            changes.push(Change {
                path: entry.path.clone(),
                before: Some(before.into_bytes()),
                after: after.into_bytes(),
            });
        }
    }
    if kind == Kind::Lean {
        for relative in lean_files(dir, base).await? {
            let source = checked_file(dir, &relative)?;
            let path = checked_file(project, &relative)?;
            let after = current(&source)?
                .with_context(|| format!("Lean ファイルの削除は手動で取り込む: {relative}"))?;
            let before = git(dir, &["show", &format!("{base}:{relative}")])
                .await
                .ok();
            let change = Change {
                path,
                before,
                after,
            };
            change.check()?;
            changes.push(change);
            has_output = true;
        }
    }
    ensure!(has_output, "取り込める成果がない");
    Ok(Changes(changes))
}
