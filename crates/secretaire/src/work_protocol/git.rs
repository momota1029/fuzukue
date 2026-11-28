//! Read captured objects and build append-only commits with Git plumbing.

use anyhow::{bail, ensure, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{wire, Event, Route, Stream};

const TREE_LIMIT: usize = wire::MAX_EVENTS * 192;
const MAX_COMMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Oid(String);

impl Oid {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(wire::valid_oid(value), "expected a full Git object ID");
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A prepared write with an exact destination, previous tip, and new commit.
/// The new commit contains the queued event bytes and has the previous tip as
/// its only parent, or is a root commit when creating the branch.
#[derive(Debug)]
pub struct PublishPlan {
    pub(super) route: Route,
    pub(super) stream: Stream,
    pub(super) previous: Option<Oid>,
    pub(super) tip: Oid,
    pub(super) events: Vec<Event>,
}

impl PublishPlan {
    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn branch(&self) -> String {
        self.route.branch(self.stream)
    }

    pub fn previous(&self) -> Option<&Oid> {
        self.previous.as_ref()
    }

    pub fn tip(&self) -> &Oid {
        &self.tip
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

/// Git object access in a trusted local repository. Reads are pinned to full
/// OIDs supplied by the capture layer and leave the worktree and index alone.
pub struct GitRepo {
    path: PathBuf,
}

impl GitRepo {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Send the exact prepared commit if the caller confirms the current
    /// approval. The lease also covers deletion and races at the destination.
    /// Reconcile a fresh capture after every attempt, even if this returns an
    /// error. Publishing alone does not advance the outbox.
    pub(super) fn publish(
        &self,
        plan: &PublishPlan,
        approve: impl FnOnce(&PublishPlan) -> Result<()>,
    ) -> Result<()> {
        let commit = self.commit(&plan.tip)?;
        ensure!(commit.parent.as_ref() == plan.previous(), "prepared commit has a different parent");
        approve(plan)?;
        let branch = plan.branch();
        let old = plan.previous().map(Oid::as_str).unwrap_or("");
        let lease = format!("--force-with-lease={branch}:{old}");
        let refspec = format!("{}:{branch}", plan.tip().as_str());
        self.run(
            &["push", "--porcelain", "--no-verify", "--no-follow-tags", "--no-signed", &lease, "--",
                plan.route().destination(), &refspec],
            &[],
            8192,
        )?;
        Ok(())
    }

    pub(super) fn history(&self, previous: Option<&Oid>, tip: &Oid) -> Result<Vec<Oid>> {
        if previous == Some(tip) {
            return Ok(Vec::new());
        }
        let limit = format!("--max-count={}", wire::MAX_EVENTS + 1);
        let mut args = vec!["rev-list", "--first-parent", &limit, tip.as_str()];
        let excluded = previous.map(|oid| format!("^{}", oid.as_str()));
        if let Some(excluded) = &excluded {
            args.push(excluded);
        }
        let raw = self.run(&args, &[], (wire::MAX_EVENTS + 1) * 65)?;
        let mut history: Vec<Oid> = raw.lines().map(Oid::parse).collect::<Result<_>>()?;
        ensure!(
            !history.is_empty() && history.first() == Some(tip),
            "stream tip moved backwards or is not a commit"
        );
        ensure!(history.len() <= wire::MAX_EVENTS, "stream has too many commits");

        history.reverse();
        ensure!(
            self.commit(&history[0])?.parent.as_ref() == previous,
            "stream history no longer extends the accepted tip"
        );
        Ok(history)
    }

    pub(super) fn read_append(
        &self,
        route: &Route,
        stream: Stream,
        oid: &Oid,
        previous: Option<&Oid>,
    ) -> Result<Vec<(u64, String)>> {
        let commit = self.commit(oid)?;
        ensure!(commit.parent.as_ref() == previous, "stream has a discontinuous parent chain");
        let tree = self.protocol_tree(&commit.tree)?;
        let mut entries = match previous {
            Some(previous) => {
                let old = self.protocol_tree(&self.commit(previous)?.tree)?;
                ensure!(tree.header == old.header, "protocol header changed");
                self.additions(&old.events, &tree.events)?
            }
            None => {
                let raw = self.blob(&tree.header, wire::MAX_HEADER_BYTES)?;
                let header: wire::Header = serde_json::from_str(&raw).context("invalid protocol header")?;
                ensure!(header == route.header(stream), "unsupported protocol version or header route");
                self.entries(&tree.events, TREE_LIMIT)?
            }
        };

        ensure!(!entries.is_empty(), "commit does not append any events");
        ensure!(entries.len() <= wire::MAX_EVENTS, "commit has too many events");
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        let mut bytes = 0;
        entries.into_iter().map(|entry| {
            ensure!(entry.mode == "100644", "event must be a regular data file");
            let stem = entry.path.strip_suffix(".json").context("unexpected event path")?;
            ensure!(
                stem.len() == 20 && stem.bytes().all(|byte| byte.is_ascii_digit()),
                "unexpected event path"
            );
            let seq = stem.parse().context("invalid event file sequence")?;
            let raw = self.blob(&entry.oid, wire::MAX_EVENT_BYTES)?;
            bytes += raw.len();
            ensure!(bytes <= super::store::MAX_STREAM_BYTES, "event batch exceeds the stream byte limit");
            Ok((seq, raw))
        }).collect()
    }

    pub(super) fn build_commit(
        &self,
        plan: &Route,
        stream: Stream,
        previous: Option<&Oid>,
        events: &[(Event, &str)],
    ) -> Result<Oid> {
        let (header, mut entries) = match previous {
            Some(previous) => {
                let tree = self.protocol_tree(&self.commit(previous)?.tree)?;
                (tree.header, self.entries(&tree.events, TREE_LIMIT)?)
            }
            None => (
                self.hash_blob(&serde_json::to_vec(&plan.header(stream))?)?,
                Vec::new(),
            ),
        };
        for (event, raw) in events {
            entries.push(Entry {
                path: format!("{:020}.json", event.seq),
                mode: "100644".into(),
                oid: self.hash_blob(raw.as_bytes())?,
            });
        }
        let event_tree = self.make_tree(&entries)?;
        let root = self.make_tree(&[
            Entry { path: "events".into(), mode: "040000".into(), oid: event_tree },
            Entry { path: "protocol.json".into(), mode: "100644".into(), oid: header },
        ])?;
        let date = chrono::DateTime::parse_from_rfc3339(&events[0].0.at)?.timestamp().max(0);
        let date = format!("@{date} +0000");
        let mut args = vec![
            "commit-tree", root.as_str(), "--no-gpg-sign", "-m", "Append fuzukue v1 events",
        ];
        if let Some(previous) = previous {
            args.extend(["-p", previous.as_str()]);
        }
        let mut command = self.command();
        command
            .env("GIT_AUTHOR_NAME", "fuzukue")
            .env("GIT_AUTHOR_EMAIL", "protocol@fuzukue.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", "fuzukue")
            .env("GIT_COMMITTER_EMAIL", "protocol@fuzukue.invalid")
            .env("GIT_COMMITTER_DATE", date);
        Oid::parse(run(command, &args, &[], 65)?.trim_end())
    }

    fn additions(&self, old: &Oid, new: &Oid) -> Result<Vec<Entry>> {
        let raw = self.run(
            &["diff-tree", "--no-commit-id", "--no-renames", "--no-abbrev", "--raw",
                "-r", "-t", "-z", old.as_str(), new.as_str()],
            &[],
            TREE_LIMIT,
        )?;
        let mut fields = raw.split_terminator('\0');
        let mut entries = Vec::new();
        while let Some(metadata) = fields.next() {
            let path = fields.next().context("missing path in Git diff")?;
            let values: Vec<_> = metadata.split_ascii_whitespace().collect();
            ensure!(
                values.len() == 5 && values[0] == ":000000" && values[4] == "A",
                "stream modified or removed a published event"
            );
            entries.push(Entry {
                path: path.into(),
                mode: values[1].into(),
                oid: Oid::parse(values[3])?,
            });
        }
        Ok(entries)
    }

    fn commit(&self, oid: &Oid) -> Result<Commit> {
        let raw = self.object("commit", oid, MAX_COMMIT_BYTES)?;
        let mut lines = raw.lines();
        let tree = lines.next().and_then(|line| line.strip_prefix("tree "))
            .context("missing commit tree")?;
        let mut parents = lines.take_while(|line| line.starts_with("parent "));
        let parent = parents.next().map(|line| Oid::parse(&line["parent ".len()..])).transpose()?;
        ensure!(parents.next().is_none(), "merge commits are forbidden on a protocol stream");
        Ok(Commit { tree: Oid::parse(tree)?, parent })
    }

    fn protocol_tree(&self, tree: &Oid) -> Result<ProtocolTree> {
        let mut entries = self.entries(tree, 512)?.into_iter();
        let events = entries.next().context("missing events directory")?;
        let header = entries.next().context("missing protocol header")?;
        ensure!(
            events.path == "events" && events.mode == "040000"
                && header.path == "protocol.json" && header.mode == "100644"
                && entries.next().is_none(),
            "unexpected protocol root paths or modes"
        );
        Ok(ProtocolTree { events: events.oid, header: header.oid })
    }

    fn entries(&self, tree: &Oid, limit: usize) -> Result<Vec<Entry>> {
        let raw = self.run(&["ls-tree", "-z", tree.as_str()], &[], limit)?;
        raw.split_terminator('\0').map(|record| {
            let (metadata, path) = record.split_once('\t').context("invalid Git tree entry")?;
            let values: Vec<_> = metadata.split_ascii_whitespace().collect();
            ensure!(values.len() == 3, "invalid Git tree entry fields");
            Ok(Entry { path: path.into(), mode: values[0].into(), oid: Oid::parse(values[2])? })
        }).collect()
    }

    fn make_tree(&self, entries: &[Entry]) -> Result<Oid> {
        let mut input = String::new();
        for entry in entries {
            let kind = if entry.mode == "040000" { "tree" } else { "blob" };
            input.push_str(&format!("{} {kind} {}\t{}\0", entry.mode, entry.oid.as_str(), entry.path));
        }
        Oid::parse(self.run(&["mktree", "-z"], input.as_bytes(), 65)?.trim_end())
    }

    fn hash_blob(&self, raw: &[u8]) -> Result<Oid> {
        Oid::parse(self.run(&["hash-object", "-w", "--stdin"], raw, 65)?.trim_end())
    }

    fn blob(&self, oid: &Oid, limit: usize) -> Result<String> {
        self.object("blob", oid, limit)
    }

    fn object(&self, kind: &str, oid: &Oid, limit: usize) -> Result<String> {
        let actual_kind = self.run(&["cat-file", "-t", oid.as_str()], &[], 16)?;
        ensure!(actual_kind.trim_end() == kind, "unexpected Git object type");
        self.run(&["cat-file", kind, oid.as_str()], &[], limit)
    }

    fn command(&self) -> Command {
        command(&self.path)
    }

    fn run(&self, args: &[&str], input: &[u8], limit: usize) -> Result<String> {
        run(self.command(), args, input, limit)
    }
}

struct Commit {
    tree: Oid,
    parent: Option<Oid>,
}

struct ProtocolTree {
    events: Oid,
    header: Oid,
}

struct Entry {
    mode: String,
    path: String,
    oid: Oid,
}

fn command(path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(path)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_RECURSIVE_SUBMODULES", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.fsync=loose-object", "-c", "push.recurseSubmodules=no",
            "-c", "protocol.ext.allow=never"]);
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE"] {
        command.env_remove(name);
    }
    command
}

fn run(mut command: Command, args: &[&str], input: &[u8], limit: usize) -> Result<String> {
    let mut child = command.args(args)
        .stdin(if input.is_empty() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start git {}", args[0]))?;
    let result = (|| -> Result<Vec<u8>> {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input)?;
        }
        let mut bytes = Vec::new();
        child.stdout.take().context("missing Git stdout")?
            .take((limit + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "git {} output exceeds the size limit", args[0]);
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let bytes = result?;
    if !status.success() {
        bail!("git {} failed ({status})", args[0]);
    }
    String::from_utf8(bytes).context("Git output contains invalid UTF-8")
}
