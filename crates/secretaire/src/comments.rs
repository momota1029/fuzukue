//! 論文（デッキ）に紐づく、送信前のコメント。
//!
//! 論文の画面でカードごとにためておき、「送信」でまとめてチャットに 1 通として送り、送ったら消す。
//! 置き場所は `<デッキ>/.fuzukue/comments.json`（送るまでの下書きなので、git には入れない想定）。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fuzukue_proto::CommentView;

pub(crate) fn file(deck: &Path) -> PathBuf {
    deck.join(".fuzukue").join("comments.json")
}

fn checked_file(path: PathBuf) -> Result<PathBuf> {
    for (component, directory) in [(path.parent().unwrap(), true), (path.as_path(), false)] {
        match component.symlink_metadata() {
            Ok(metadata) => anyhow::ensure!(
                if directory { metadata.is_dir() } else { metadata.is_file() },
                "Invalid comment storage: {}", component.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub fn load(deck: &Path) -> Vec<CommentView> {
    read(deck).unwrap_or_default()
}

pub fn read(deck: &Path) -> Result<Vec<CommentView>> {
    let path = checked_file(fuzukue_proto::compat::workspace_data(deck, "comments.json"))?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} を読めない", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("{} を読めない", path.display())),
    }
}

pub fn save(deck: &Path, items: &[CommentView]) -> Result<()> {
    let f = checked_file(file(deck))?;
    // An empty current file shadows any legacy drafts already sent or cleared.
    if items.is_empty() && !deck.join(".labo/comments.json").exists() {
        match std::fs::remove_file(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }
    std::fs::create_dir_all(f.parent().unwrap())?;
    // .fuzukue/ は下書きなので、デッキの git に入らないようにしておく。
    let ignore = f.parent().unwrap().join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "*\n")?;
    }
    let tmp = tempfile::NamedTempFile::new_in(f.parent().unwrap())?;
    std::fs::write(tmp.path(), serde_json::to_vec_pretty(items)?)?;
    crate::durable::persist(tmp, f)?;
    Ok(())
}

/// まとめて送るときの 1 通の文面。
pub fn compose(deck_title: &str, deck_rel: &str, items: &[CommentView]) -> String {
    let mut s = harness_format!("Comments on “{deck_title}” (`{deck_rel}`), total {}. Address each and tell the user which cards you edited.\n", "論文「{deck_title}」（`{deck_rel}`）へのコメントが {} 件ある。それぞれ対応して、直したカードを教えて。\n", items.len());
    for (i, c) in items.iter().enumerate() {
        let text = c.text.trim();
        let number = i + 1;
        let indent = " ".repeat(number.to_string().len() + 2);
        // Manuscript text outside the cards has no card link. Name its file and
        // always include the TeX that the comment is anchored to.
        let manuscript = fuzukue_proto::manuscript_comment_path(&c.card);
        let place = match manuscript {
            Some("") => harness_format!("Manuscript text outside the cards", "カード外の原稿の本文"),
            Some(path) => harness_format!("Manuscript text outside the cards, in `{path}`", "カード外の原稿の本文（`{path}`）"),
            None => format!("[[{}]]", c.card),
        };
        let quote = c.quote.as_deref().map(str::trim).filter(|q| !q.is_empty());
        if let Some(quote) = quote {
            s.push_str(&harness_format!("\n{number}. {place}\n{indent}Quote:", "\n{number}. {place}\n{indent}引用:"));
            if quote.contains('\n') {
                // Indent the Markdown quote as a child of this list item.
                // Display math needs its fences on their own lines.
                s.push('\n');
                for line in quote.lines() {
                    s.push_str(&format!("\n{indent}> {line}"));
                }
                s.push('\n');
            } else {
                s.push(' ');
                s.push_str(quote);
            }
        } else if manuscript.is_some() {
            s.push_str(&format!("\n{number}. {place}"));
        } else {
            s.push_str(&format!("\n{number}. {place}: {text}"));
            continue;
        }
        if let Some(source) = c.source.as_ref().map(|source| source.text.trim()) {
            if !source.is_empty() && Some(source) != quote {
                let source = source.replace('\n', &format!("\n{indent}  "));
                s.push_str(&harness_format!("\n{indent}TeX source: {source}", "\n{indent}TeX 原文: {source}"));
            }
        }
        s.push_str(&harness_format!("\n{indent}Comment: {text}", "\n{indent}コメント: {text}"));
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn manuscript_comments_persist_and_name_their_file_and_text() -> anyhow::Result<()> {
        let draft = |path: &str, text: &str, quote: Option<&str>, note: &str| super::CommentView {
            card: format!("{}{path}", fuzukue_proto::MANUSCRIPT_COMMENT_PREFIX),
            text: note.into(),
            quote: quote.map(str::to_owned),
            proof: quote.map(|_| false),
            source: Some(fuzukue_proto::CommentSource { range: 0..text.len(), text: text.into(), context: None }),
            created: 1,
        };
        let items = [
            draft("paper/abstract.tex", "We study spheres.", None, "Shorten the abstract."),
            draft("paper/intro.tex", "widgets", Some("widgets"), "Define them."),
            draft("", "Closing prose.", None, "Add a reference."),
        ];
        let deck = tempfile::tempdir()?;
        super::save(deck.path(), &items)?;
        let loaded = super::read(deck.path())?;
        assert_eq!(loaded, items);
        let message = super::compose("Paper", "paper", &loaded);
        assert!(!message.contains("[["), "{message}");
        assert!(message.contains(harness_text!(
            "1. Manuscript text outside the cards, in `paper/abstract.tex`\n   TeX source: We study spheres.\n   Comment: Shorten the abstract.",
            "1. カード外の原稿の本文（`paper/abstract.tex`）\n   TeX 原文: We study spheres.\n   コメント: Shorten the abstract."
        )), "{message}");
        assert!(message.contains(harness_text!(
            "2. Manuscript text outside the cards, in `paper/intro.tex`\n   Quote: widgets\n   Comment: Define them.",
            "2. カード外の原稿の本文（`paper/intro.tex`）\n   引用: widgets\n   コメント: Define them."
        )), "{message}");
        assert!(message.contains(harness_text!(
            "3. Manuscript text outside the cards\n   TeX source: Closing prose.",
            "3. カード外の原稿の本文\n   TeX 原文: Closing prose."
        )), "{message}");
        Ok(())
    }

    #[test]
    fn compose_keeps_display_math_as_its_own_block() {
        let comment = super::CommentView {
            card: "lemma".into(),
            text: "Explain the equality.".into(),
            quote: Some("Cost \\$5.\n\n$$\nx=y\n$$".into()),
            proof: None,
            source: None,
            created: 0,
        };
        let message = super::compose("Paper", "paper", &[comment]);
        assert!(message.contains(
            harness_text!("1. [[lemma]]\n   Quote:\n\n   > Cost \\$5.\n   > \n   > $$\n   > x=y\n   > $$\n\n   Comment: Explain the equality.",
            "1. [[lemma]]\n   引用:\n\n   > Cost \\$5.\n   > \n   > $$\n   > x=y\n   > $$\n\n   コメント: Explain the equality.")
        ));
    }
}

pub(crate) struct Application {
    pub origin: String,
    pub notice: fuzukue_proto::CommentApplication,
    pub root: PathBuf,
    pub before: Result<crate::auto_review::DeckFiles>,
}

impl Application {
    pub fn finish(mut self, outcome: Result<()>) -> fuzukue_proto::ServerEvent {
        use fuzukue_proto::{CommentApplicationState, ServerEvent};

        let changed = outcome.and_then(|()| {
            let before = self.before?;
            let after = crate::auto_review::card_files(&fuzukue_cards::Graph::open_local(&self.root)?)?;
            let cards = before.keys().chain(after.keys())
                .filter(|&id| before.get(id) != after.get(id))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            Ok(cards.into_iter().collect())
        });
        self.notice.state = match changed {
            Ok(cards) => CommentApplicationState::Applied { cards },
            Err(error) => CommentApplicationState::Failed { reason: format!("{error:#}") },
        };
        ServerEvent::CommentApplication { session: self.origin, application: self.notice }
    }
}
