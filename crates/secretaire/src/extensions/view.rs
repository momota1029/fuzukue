use std::{fs, path::Path};

use anyhow::{ensure, Context, Result};
use fluent_bundle::{concurrent::FluentBundle, FluentResource};
use fuzukue_proto::{ExtensionNode, ExtensionTab};
use serde::Deserialize;

type Bundle = FluentBundle<FluentResource>;

#[derive(Default)]
pub(super) struct Locales {
    en: Option<Bundle>,
    ja: Option<Bundle>,
}

impl Locales {
    pub(super) fn load(root: &Path) -> Result<Self> {
        fn bundle(root: &Path, locale: &str) -> Result<Option<Bundle>> {
            let dir = root.join("locales").join(locale);
            if !dir.try_exists()? {
                return Ok(None);
            }
            let mut bundle = Bundle::new_concurrent(vec![locale.parse()?]);
            bundle.set_use_isolating(false);
            let mut files = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
            files.sort_by_key(|file| file.file_name());
            for file in files {
                if file.path().extension().is_some_and(|ext| ext == "ftl") {
                    let metadata = file.path().symlink_metadata()?;
                    ensure!(
                        metadata.is_file() && metadata.len() <= 256 * 1024,
                        "invalid extension locale file"
                    );
                    let resource = FluentResource::try_new(fs::read_to_string(file.path())?)
                        .map_err(|(_, errors)| {
                            anyhow::anyhow!("invalid Fluent catalog: {errors:?}")
                        })?;
                    bundle
                        .add_resource(resource)
                        .map_err(|errors| anyhow::anyhow!("invalid Fluent messages: {errors:?}"))?;
                }
            }
            Ok(Some(bundle))
        }
        Ok(Self {
            en: bundle(root, "en")?,
            ja: bundle(root, "ja")?,
        })
    }

    pub(super) fn text(&self, key: &str, locale: &str) -> String {
        let bundles = if locale == "en" {
            [&self.en, &self.ja]
        } else {
            [&self.ja, &self.en]
        };
        for bundle in bundles.into_iter().flatten() {
            if let Some(value) = bundle.get_message(key).and_then(|message| message.value()) {
                let mut errors = Vec::new();
                let resolved = bundle.format_pattern(value, None, &mut errors);
                if errors.is_empty() {
                    return resolved.into_owned();
                }
            }
        }
        key.into()
    }

    pub(super) fn tree(&self, json: &str, locale: &str) -> Result<Vec<ExtensionNode>> {
        ensure!(json.len() <= 256 * 1024, "extension view exceeds 256 KiB");
        let nodes = serde_json::from_str::<Vec<Node>>(json).context("invalid extension view")?;
        let mut budget = 2048;
        let mut ids = std::collections::BTreeSet::new();
        self.nodes(nodes, locale, 0, &mut budget, &mut ids)
    }

    fn nodes(
        &self,
        nodes: Vec<Node>,
        locale: &str,
        depth: usize,
        budget: &mut usize,
        ids: &mut std::collections::BTreeSet<String>,
    ) -> Result<Vec<ExtensionNode>> {
        ensure!(depth <= 8, "extension view is too deeply nested");
        let text = |text: Text| match text {
            Text::Literal(text) => text,
            Text::Key { key } => self.text(&key, locale),
        };
        let mut result = Vec::new();
        for node in nodes {
            let size = match &node {
                Node::Table { columns, rows } => {
                    columns.len() + rows.iter().map(Vec::len).sum::<usize>() + rows.len()
                }
                Node::List { items } => items.len(),
                Node::Tabs { tabs, .. } => tabs.len(),
                _ => 0,
            };
            *budget = budget
                .checked_sub(size + 1)
                .context("extension view is too large")?;
            if let Node::Input { id, .. } | Node::Button { id, .. } | Node::Tabs { id, .. } = &node
            {
                ensure!(
                    super::identifier(id) && ids.insert(id.clone()),
                    "invalid or duplicate view element ID"
                );
            }
            result.push(match node {
                Node::Heading { text: label } => ExtensionNode::Heading { text: text(label) },
                Node::Markdown { text: label } => ExtensionNode::Markdown { text: text(label) },
                Node::Table { columns, rows } => ExtensionNode::Table {
                    columns: columns.into_iter().map(text).collect(),
                    rows: rows
                        .into_iter()
                        .map(|row| row.into_iter().map(text).collect())
                        .collect(),
                },
                Node::List { items } => ExtensionNode::List {
                    items: items.into_iter().map(text).collect(),
                },
                Node::Input { id, label, value } => ExtensionNode::Input {
                    id,
                    label: text(label),
                    value,
                },
                Node::Button { id, text: label } => ExtensionNode::Button {
                    id,
                    text: text(label),
                },
                Node::Tabs { id, tabs } => ExtensionNode::Tabs {
                    id,
                    tabs: tabs
                        .into_iter()
                        .map(|tab| {
                            Ok(ExtensionTab {
                                title: text(tab.title),
                                children: self.nodes(
                                    tab.children,
                                    locale,
                                    depth + 1,
                                    budget,
                                    ids,
                                )?,
                            })
                        })
                        .collect::<Result<_>>()?,
                },
            });
        }
        Ok(result)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Text {
    Literal(String),
    Key { key: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Node {
    Heading {
        text: Text,
    },
    Markdown {
        text: Text,
    },
    Table {
        columns: Vec<Text>,
        rows: Vec<Vec<Text>>,
    },
    List {
        items: Vec<Text>,
    },
    Input {
        id: String,
        label: Text,
        #[serde(default)]
        value: String,
    },
    Button {
        id: String,
        text: Text,
    },
    Tabs {
        id: String,
        tabs: Vec<Tab>,
    },
}

#[derive(Deserialize)]
struct Tab {
    title: Text,
    children: Vec<Node>,
}
