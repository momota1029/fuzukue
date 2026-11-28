//! Preserve suspect files found by the post-job worktree comparison.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::touches::WriteSet;
use crate::mcp::{workspace_read, workspace_write};

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_JOB: u64 = 128 * 1024 * 1024;
const REPORT_LIMIT: usize = 12;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    file: bool,
    size: u64,
    device: u64,
    inode: u64,
    mode: u32,
    times: [i64; 4],
}

impl From<&fs::Metadata> for Fingerprint {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            file: meta.is_file(),
            size: meta.len(),
            device: meta.dev(),
            inode: meta.ino(),
            mode: meta.mode(),
            times: [
                meta.mtime(),
                meta.mtime_nsec(),
                meta.ctime(),
                meta.ctime_nsec(),
            ],
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Action {
    Copied { backup: String },
    Missing,
    Protected,
    Skipped { reason: &'static str },
    Failed { error: String },
}

#[derive(Serialize)]
struct Entry {
    path: String,
    #[serde(flatten)]
    action: Action,
}

/// What is known about the writer of a change observed in a checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Writer {
    /// The job's own tool calls or backend edits wrote the path.
    Own,
    /// Another job wrote the path or declared it.
    Other,
    Unconfirmed,
}

impl Writer {
    pub(super) fn label(self, japanese: bool) -> &'static str {
        match (self, japanese) {
            (Self::Own, false) => "confirmed own",
            (Self::Own, true) => "この仕事の書き込みと確認",
            (Self::Other, false) => "declared or written by another job",
            (Self::Other, true) => "ほかの仕事が予定したか書き込んだ",
            (Self::Unconfirmed, false) => "writer unconfirmed",
            (Self::Unconfirmed, true) => "書き手は未確認",
        }
    }
}

/// The writer of each observed change, and the changes that reports only count.
#[derive(Default)]
pub(super) struct Attribution {
    writers: BTreeMap<PathBuf, Writer>,
    summarized: BTreeSet<PathBuf>,
}

impl Attribution {
    /// Label every path. Only files in `present` under `target/` that git
    /// ignores, and that this job is not known to have written, are summarized.
    /// Deletions, tracked files and protected paths stay listed.
    pub(super) fn new<'a>(
        root: &Path,
        paths: impl Iterator<Item = &'a PathBuf>,
        present: &BTreeMap<PathBuf, Fingerprint>,
        writer: impl Fn(&Path) -> Writer,
    ) -> Self {
        let writers: BTreeMap<_, _> = paths.map(|path| (path.clone(), writer(path))).collect();
        let protected = protection();
        // The outer repository calls a nested repository's tracked files ignored.
        let nested = |path: &Path| {
            path.ancestors().skip(1).filter(|dir| !dir.as_os_str().is_empty())
                .any(|dir| root.join(dir).join(".git").symlink_metadata().is_ok())
        };
        let candidates: Vec<_> = writers.iter()
            .filter(|(path, writer)| {
                path.starts_with("target") && **writer != Writer::Own && present.contains_key(*path)
                    && !protected(&root.join(path)) && !nested(path)
            })
            .map(|(path, _)| path.as_path())
            .collect();
        let summarized = crate::git::ignored(root, &candidates);
        Self { writers, summarized }
    }

    pub(super) fn writer(&self, path: &Path) -> Writer {
        self.writers.get(path).copied().unwrap_or(Writer::Unconfirmed)
    }

    pub(super) fn summarized(&self, path: &Path) -> bool {
        self.summarized.contains(path)
    }

    /// One line for each directory and writer among the summarized `paths`.
    pub(super) fn summaries<'a>(&self, paths: impl Iterator<Item = &'a Path>, japanese: bool) -> Vec<String> {
        let mut counts = BTreeMap::<(PathBuf, Writer), usize>::new();
        for path in paths.filter(|path| self.summarized(path)) {
            // `target/<name>`, or `target` for the files directly in it.
            let depth = if path.components().count() > 2 { 2 } else { 1 };
            let directory = path.components().take(depth).collect();
            *counts.entry((directory, self.writer(path))).or_default() += 1;
        }
        counts.into_iter().map(|((directory, writer), count)| {
            let directory = directory.display();
            let writer = writer.label(japanese);
            if japanese {
                format!("{directory}/ の git が無視する未追跡ファイル {count} 件（{writer}）")
            } else {
                format!("{directory}/: {count} untracked ignored files ({writer})")
            }
        }).collect()
    }
}

fn protection() -> impl Fn(&Path) -> bool {
    let aliases = crate::folder_grants::protected_aliases();
    move |path| workspace_write::denied(path) || aliases.iter().any(|alias| path.starts_with(alias))
}

#[derive(Default)]
pub(super) struct Review {
    entries: Vec<Entry>,
    manifest: Option<PathBuf>,
    error: Option<String>,
    incomplete: bool,
    unknown: bool,
}

pub(super) fn review(
    root: &Path,
    recovery: &Path,
    writes: &WriteSet,
    changed: &[PathBuf],
    after: &BTreeMap<PathBuf, Fingerprint>,
    incomplete: bool,
) -> Review {
    let mut review = Review {
        entries: Vec::new(),
        manifest: None,
        error: None,
        incomplete,
        unknown: !writes.known() || root != writes.root(),
    };
    let protected = protection();
    let unknown = review.unknown;
    let candidates = changed
        .iter()
        .filter(|path| unknown || !writes.contains(path) || protected(&root.join(path)));
    let directory = recovery.join("quarantine");
    let mut remaining = MAX_JOB;
    for path in candidates {
        let source = root.join(path);
        let number = review.entries.len() + 1;
        let action = match after.get(path) {
            _ if protected(&source) => Action::Protected,
            None if incomplete => Action::Skipped {
                reason: "incomplete scan",
            },
            None => Action::Missing,
            Some(expected) if !expected.file => Action::Skipped {
                reason: "not a regular file",
            },
            Some(expected) if expected.size > MAX_FILE || expected.size > remaining => {
                Action::Skipped {
                    reason: "copy size limit",
                }
            }
            Some(expected) => {
                let backup = format!("{number:06}.file");
                match copy(&source, recovery, &directory, &backup, expected) {
                    Ok(()) => {
                        remaining -= expected.size;
                        Action::Copied { backup }
                    }
                    Err(error) => Action::Failed {
                        error: format!("{error:#}"),
                    },
                }
            }
        };
        review.entries.push(Entry {
            path: path.to_string_lossy().into_owned(),
            action,
        });
    }
    if !review.entries.is_empty() || incomplete {
        let manifest = directory.join("manifest.json");
        let save = || -> Result<()> {
            prepare(recovery, &directory)?;
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            serde_json::to_writer_pretty(
                &mut file,
                &serde_json::json!({
                    "incomplete": incomplete,
                    "unknown_write_set": review.unknown,
                    "worktree_action": "left_in_place",
                    "entries": review.entries,
                }),
            )?;
            file.as_file().sync_all()?;
            file.persist_noclobber(&manifest)?;
            Ok(())
        };
        match save() {
            Ok(()) => review.manifest = Some(manifest),
            Err(error) => review.error = Some(format!("{error:#}")),
        }
    }
    review
}

fn prepare(recovery: &Path, directory: &Path) -> Result<()> {
    workspace_write::prepare_recovery(recovery)?;
    workspace_write::private_dir(directory)
}

fn copy(
    source: &Path,
    recovery: &Path,
    directory: &Path,
    backup: &str,
    expected: &Fingerprint,
) -> Result<()> {
    let mut input = workspace_read::open_regular_source(source)?;
    ensure!(
        Fingerprint::from(&input.metadata()?) == *expected,
        "The file changed after the scan"
    );
    prepare(recovery, directory)?;
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    let size = io::copy(&mut (&mut input).take(expected.size + 1), &mut output)?;
    ensure!(
        size == expected.size && Fingerprint::from(&input.metadata()?) == *expected,
        "The file changed during the copy"
    );
    output.as_file().sync_all()?;
    output
        .persist_noclobber(directory.join(backup))
        .context("Could not save the quarantine copy")?;
    Ok(())
}

impl Review {
    pub(super) fn render(&self, attribution: &Attribution, japanese: bool) -> String {
        if self.entries.is_empty() && !self.incomplete {
            return String::new();
        }
        let mut lines = vec![
            if japanese {
                "\n\n## 範囲外の書き込み"
            } else {
                "\n\n## Out-of-scope writes"
            }
            .to_owned(),
        ];
        if self.unknown {
            lines.push(if japanese { "変更予定を特定できなかったため、観測した変更をすべて確認対象にした。" }
                else { "The effective write set is unknown. All observed changes are listed for review." }.into());
        }
        if self.incomplete {
            lines.push(
                if japanese {
                    "一部のファイルは検査できなかった。"
                } else {
                    "The worktree scan was incomplete."
                }
                .into(),
            );
        }
        // Worktrees can be shared with users and other jobs. A before/after
        // comparison cannot identify the writer, even for a tracked file.
        lines.push(
            if japanese {
                "並行作業の内容を守るため、作業ツリーは検査時の状態を保持した。"
            } else {
                "The worktree was left as observed to preserve concurrent changes."
            }
            .into(),
        );
        let listed: Vec<_> = self.entries.iter()
            .filter(|entry| !attribution.summarized(Path::new(&entry.path)))
            .collect();
        for entry in listed.iter().take(REPORT_LIMIT) {
            let action = match (&entry.action, japanese) {
                (Action::Copied { backup }, false) => format!("copied to quarantine ({backup})"),
                (Action::Copied { backup }, true) => format!("隔離先にコピーした（{backup}）"),
                (Action::Missing, false) => "missing after the job; needs review".into(),
                (Action::Missing, true) => "終了時に見つからなかった。確認が必要".into(),
                (Action::Protected, false) => "protected path; needs review".into(),
                (Action::Protected, true) => "保護対象のパス。確認が必要".into(),
                (Action::Skipped { reason }, false) => {
                    format!("copy skipped ({reason}); needs review")
                }
                (Action::Skipped { reason }, true) => {
                    format!("コピーを省略した（{reason}）。確認が必要")
                }
                (Action::Failed { error }, false) => format!("copy failed ({error}); needs review"),
                (Action::Failed { error }, true) => {
                    format!("コピーに失敗した（{error}）。確認が必要")
                }
            };
            let writer = attribution.writer(Path::new(&entry.path)).label(japanese);
            lines.push(if japanese {
                format!("- {:?} — {action}（{writer}）", entry.path)
            } else {
                format!("- {:?} — {action} ({writer})", entry.path)
            });
        }
        let more = |remaining| {
            if japanese {
                format!("ほか {remaining} 件。")
            } else {
                format!("{remaining} more paths.")
            }
        };
        if listed.len() > REPORT_LIMIT {
            lines.push(more(listed.len() - REPORT_LIMIT));
        }
        let summaries = attribution.summaries(self.entries.iter().map(|entry| Path::new(&entry.path)), japanese);
        lines.extend(summaries.iter().take(REPORT_LIMIT).map(|summary| format!("- {summary}")));
        if summaries.len() > REPORT_LIMIT {
            let remaining = summaries.len() - REPORT_LIMIT;
            lines.push(if japanese {
                format!("ほか {remaining} ディレクトリー。")
            } else {
                format!("{remaining} more directories.")
            });
        }
        if let Some(manifest) = &self.manifest {
            lines.push(if japanese {
                format!("隔離の記録とコピーの保存先は {manifest:?}。")
            } else {
                format!("Quarantine copies and the full manifest are at {manifest:?}.")
            });
        }
        if let Some(error) = &self.error {
            lines.push(if japanese {
                format!("隔離の記録を保存できなかった（{error}）。")
            } else {
                format!("Could not save the quarantine manifest ({error}).")
            });
        }
        lines.join("\n")
    }
}
