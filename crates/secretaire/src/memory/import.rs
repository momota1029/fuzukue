//! Reviewed, append-only import from a user-selected Claude memory directory.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceRecord {
    path: PathBuf,
    file: String,
    revision: String,
    first_line: Option<usize>,
    last_line: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preview {
    source: PathBuf,
    file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Prepare {
    source: PathBuf,
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Choice {
    file: String,
    revision: String,
    scope: Scope,
    first_line: Option<usize>,
    last_line: Option<usize>,
    summary: Option<String>,
    body: Option<String>,
    #[serde(default)]
    worker: bool,
}

pub(super) struct Plan {
    source: PathBuf,
    snapshot: String,
    items: Items,
}

pub(super) struct Applied {
    pub(super) count: usize,
    pub(super) backup: String,
}

fn open_source(path: &Path) -> Result<(PathBuf, File)> {
    ensure!(
        path.is_absolute(),
        "choose an absolute Claude memory directory"
    );
    let dir = crate::mcp::workspace_read::open_no_symlinks(path)?;
    ensure!(
        dir.metadata()?.is_dir(),
        "the Claude memory source must be a directory"
    );
    Ok((path.canonicalize()?, dir))
}

fn markdown_name(name: &str) -> bool {
    name.ends_with(".md")
        && !name.starts_with('.')
        && !name.chars().any(char::is_control)
        && Path::new(name).file_name().is_some_and(|part| part == name)
}

fn read_source(dir: &File, name: &str) -> Result<String> {
    ensure!(
        markdown_name(name),
        "choose a Markdown filename from the preview"
    );
    let file = File::from(fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    ensure!(
        file.metadata()?.is_file(),
        "source entry must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_FILE,
        "source file exceeds the import preview size limit"
    );
    String::from_utf8(bytes).map_err(|_| anyhow!("source file is not UTF-8"))
}

fn source_body(text: &str) -> &str {
    // Claude's per-item Markdown can start with YAML metadata. The preview also
    // exposes the original line numbers for cases that need a chosen excerpt.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(frontmatter) = text.strip_prefix("---\n") {
        if let Some((_, body)) = frontmatter.split_once("\n---\n") {
            return body;
        }
    }
    if let Some(frontmatter) = text.strip_prefix("---\r\n") {
        if let Some((_, body)) = frontmatter.split_once("\r\n---\r\n") {
            return body;
        }
    }
    text
}

fn proposed_summary(body: &str) -> String {
    body.lines()
        .map(|line| line.trim().trim_start_matches(['#', '-', '*']).trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
        .take(160)
        .collect()
}

pub(super) fn preview(args: Preview) -> Result<Value> {
    let (source, dir) = open_source(&args.source)?;
    if let Some(file) = args.file {
        let body =
            read_source(&dir, &file).with_context(|| format!("read source file {file:?}"))?;
        return Ok(
            json!({"source": source, "file": file, "revision": hash(body.as_bytes()), "text": body}),
        );
    }

    let mut names = BTreeSet::new();
    let mut unavailable_names = 0;
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        match entry.file_name().to_str() {
            Ok(name) if markdown_name(name) => {
                names.insert(name.to_owned());
                ensure!(
                    names.len() <= MAX_ITEMS,
                    "source has more than 200 Markdown files; inspect and prepare selected files in batches"
                );
            }
            Err(_) => unavailable_names += 1,
            _ => {}
        }
    }
    let candidates: Vec<_> = names
        .into_iter()
        .map(|file| match read_source(&dir, &file) {
            Ok(text) => json!({
                "file": file,
                "revision": hash(text.as_bytes()),
                "lines": text.lines().count(),
                "excerpt": source_body(&text).trim().chars().take(200).collect::<String>(),
                "index_file": file == "MEMORY.md",
                "scope": null,
            }),
            Err(error) => json!({"file": file, "unavailable": format!("{error:#}")}),
        })
        .collect();
    Ok(json!({"source": source, "candidates": candidates, "unavailable_names": unavailable_names}))
}

impl Store<'_> {
    pub(super) fn prepare_import(&self, args: Prepare) -> Result<(Option<Plan>, Value)> {
        ensure!(
            !args.choices.is_empty() && args.choices.len() <= MAX_ITEMS,
            "choose between 1 and 200 import items and their destination scopes"
        );
        let (source, dir) = open_source(&args.source)?;
        self.with_items(|_, _, mut items| {
            let snapshot = Index::build(&items)?.revision;
            let mut selected = BTreeSet::new();
            let mut added = Vec::new();
            let mut duplicates = Vec::new();
            for choice in args.choices {
                ensure!(
                    choice.scope != Scope::Workspace || self.home != self.workspace,
                    "the workspace and global memory roots coincide"
                );
                let text = read_source(&dir, &choice.file)
                    .with_context(|| format!("read source file {:?}", choice.file))?;
                ensure!(
                    hash(text.as_bytes()) == choice.revision,
                    "source changed; preview it again"
                );
                let body = match (choice.first_line, choice.last_line) {
                    (None, None) => source_body(&text).trim().to_owned(),
                    (Some(first), Some(last))
                        if first > 0 && first <= last && last <= text.lines().count() =>
                    {
                        text.lines()
                            .skip(first - 1)
                            .take(last - first + 1)
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    _ => return Err(anyhow!("choose a valid pair of source line numbers")),
                };
                let body = choice.body.unwrap_or(body);
                let summary = choice.summary.unwrap_or_else(|| proposed_summary(&body));
                validate_text(&summary, &body)?;
                ensure!(
                    !body.trim().is_empty(),
                    "an imported memory must contain text"
                );
                let provenance = SourceRecord {
                    path: source.clone(),
                    file: choice.file,
                    revision: choice.revision,
                    first_line: choice.first_line,
                    last_line: choice.last_line,
                };
                let namespace = match choice.scope {
                    Scope::Global => self.home,
                    Scope::Workspace => self.workspace,
                };
                let id = import_id(&provenance, choice.scope, namespace)?;
                ensure!(
                    selected.insert(id.clone()),
                    "choose each source excerpt only once per scope"
                );
                if items.iter().any(|((scope, existing_id), item)| {
                    *scope == choice.scope
                        && (existing_id == &id || item.body.trim() == body.trim())
                }) {
                    duplicates.push(json!({"source": provenance, "scope": choice.scope}));
                    continue;
                }
                ensure!(
                    !items.keys().any(|(_, existing_id)| existing_id == &id),
                    "an imported ID conflicts with another memory"
                );
                let now = Utc::now();
                let key = (choice.scope, id.clone());
                let item = Item::new(
                    Metadata {
                        version: 1,
                        id,
                        summary,
                        created_at: now,
                        updated_at: now,
                        worker: choice.worker,
                        origin: Origin::ClaudeImport,
                        imported_from: Some(provenance),
                    },
                    &body,
                )?;
                added.push(key.clone());
                items.insert(key, item);
            }
            Index::build(&items)?;
            let planned: Items = added
                .into_iter()
                .map(|key| {
                    let item = items.remove(&key).unwrap();
                    (key, item)
                })
                .collect();
            let preview: Vec<_> = planned
                .iter()
                .map(|((scope, _), item)| {
                    json!({
                        "source": item.metadata.imported_from,
                        "scope": scope,
                        "summary": item.metadata.summary,
                        "excerpt": item.body.chars().take(200).collect::<String>(),
                        "worker": item.metadata.worker,
                    })
                })
                .collect();
            let plan = (!planned.is_empty()).then_some(Plan {
                source,
                snapshot,
                items: planned,
            });
            Ok((plan, json!({"items": preview, "duplicates": duplicates})))
        })
    }

    pub(super) fn apply_import(&self, plan: Plan) -> Result<Applied> {
        let (_, source) = open_source(&plan.source)?;
        self.with_items(|global, workspace, mut items| {
            ensure!(Index::build(&items)?.revision == plan.snapshot,
                "destination memory changed; preview the import again");
            for item in plan.items.values() {
                let provenance = item.metadata.imported_from.as_ref().unwrap();
                let text = read_source(&source, &provenance.file)
                    .with_context(|| format!("read source file {:?}", provenance.file))?;
                ensure!(hash(text.as_bytes()) == provenance.revision, "source changed; preview the import again");
            }
            let backup = import_backup(global, &items, &plan, self.workspace)?;
            let mut count = 0;
            for (key, item) in plan.items {
                let write = || -> Result<()> {
                    let workspace_owned;
                    let root = match (key.0, workspace) {
                        (Scope::Global, _) => global,
                        (Scope::Workspace, Some(root)) => root,
                        (Scope::Workspace, None) => {
                            workspace_owned = memory_dir(self.workspace, true)?.context("workspace memory is unavailable")?;
                            &workspace_owned
                        }
                    };
                    let folder = directory(root, "items", true, true)?.context("memory items are unavailable")?;
                    atomic_write(&folder, &format!("{}.md", key.1), item.raw.as_bytes(), || Ok(()), true)
                };
                write().with_context(|| format!(
                    "Import stopped after {count} completed writes. Preview the source again to resume. Private backup: {backup}"
                ))?;
                count += 1;
                items.insert(key, item);
            }
            let index = Index::build(&items)?;
            let _ = refresh_index(global, &index.global);
            if self.home != self.workspace {
                if let Ok(Some(root)) = memory_dir(self.workspace, false) {
                    let _ = refresh_index(&root, &index.workspace);
                }
            }
            Ok(Applied { count, backup })
        })
    }
}

fn import_id(source: &SourceRecord, scope: Scope, destination: &Path) -> Result<String> {
    // Stable IDs make a partially applied batch safe to preview and retry.
    let digest = hash(&serde_json::to_vec(&(source, scope, destination))?);
    Ok(format!(
        "{}-{}-8{}-8{}-{}",
        &digest[..8],
        &digest[8..12],
        &digest[13..16],
        &digest[17..20],
        &digest[20..32]
    ))
}

fn import_backup(global: &File, items: &Items, plan: &Plan, workspace: &Path) -> Result<String> {
    let backups =
        directory(global, "backups", true, true)?.context("could not create memory backups")?;
    let imports =
        directory(&backups, "imports", true, true)?.context("could not create import backups")?;
    let name = format!("{}-{}", Utc::now().format("%Y-%m-%dT%H%M%S%.fZ"), new_id());
    let folder =
        directory(&imports, &name, true, true)?.context("could not create dated import backup")?;
    for ((scope, id), item) in items {
        let layer = match scope {
            Scope::Global => "global",
            Scope::Workspace => "workspace",
        };
        let layer =
            directory(&folder, layer, true, true)?.context("could not create backup scope")?;
        atomic_write(
            &layer,
            &format!("{id}.md"),
            item.raw.as_bytes(),
            || Ok(()),
            true,
        )?;
    }
    let manifest = json!({
        "workspace": workspace,
        "snapshot": plan.snapshot,
        "planned_items": plan.items.iter().map(|((scope, id), item)| json!({
            "scope": scope, "id": id, "revision": item.revision, "source": item.metadata.imported_from,
        })).collect::<Vec<_>>(),
    });
    atomic_write(
        &folder,
        "manifest.json",
        &serde_json::to_vec_pretty(&manifest)?,
        || Ok(()),
        true,
    )?;
    Ok(format!("backups/imports/{name}"))
}
