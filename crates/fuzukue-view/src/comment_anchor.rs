//! Anchors of comments on manuscript text outside the cards.
//!
//! A passage has no card id, and its byte offsets move when the TeX changes.
//! The anchor therefore keeps the file, the offset at the time of writing and
//! the TeX text itself. The text decides where the comment belongs; the offset
//! decides among passages that are not loaded or no longer contain the text.

use std::ops::Range;

use fuzukue_proto::{
    manuscript_comment_path, CommentSource, CommentView, DocProseText, MANUSCRIPT_COMMENT_PREFIX,
};

/// Length of the excerpt that anchors a comment on a whole passage.
const EXCERPT_BYTES: usize = 240;

/// A passage on screen. `tex` is `None` until its source is loaded.
pub struct Passage<'a> {
    pub text: &'a DocProseText,
    pub tex: Option<&'a str>,
}

/// The comment id and anchor for a passage whose joined TeX is `tex`.
/// `selection` is a byte range in `tex`; without it the anchor covers the
/// beginning of the passage.
pub fn passage_comment_anchor(
    text: &DocProseText,
    tex: &str,
    selection: Option<Range<usize>>,
) -> (String, CommentSource) {
    let selection = selection.filter(|range| {
        tex.get(range.clone()).is_some_and(|text| !text.trim().is_empty())
    });
    let excerpt = match &selection {
        Some(range) => tex[range.clone()].to_owned(),
        None => head(tex).to_owned(),
    };
    let (path, range) = match text {
        DocProseText::Tex(_) => (String::new(), selection.unwrap_or(0..tex.len())),
        DocProseText::Source(spans) => {
            let mut offset = 0;
            let mut found = None;
            for span in spans {
                offset += usize::from(span.newline_before);
                let start = span.start as usize;
                match &selection {
                    Some(range) if range.start < offset + span.len => {
                        let from = range.start.saturating_sub(offset);
                        let to = range.end.saturating_sub(offset).min(span.len).max(from);
                        found = Some((span.path.clone(), start + from..start + to));
                    }
                    Some(_) => {}
                    None => found = Some((span.path.clone(), start..start + span.len)),
                }
                if found.is_some() {
                    break;
                }
                offset += span.len;
            }
            found.unwrap_or_default()
        }
    };
    (
        format!("{MANUSCRIPT_COMMENT_PREFIX}{path}"),
        CommentSource { range, text: excerpt },
    )
}

fn head(tex: &str) -> &str {
    let tex = tex.trim();
    let mut end = tex.len().min(EXCERPT_BYTES);
    while !tex.is_char_boundary(end) {
        end -= 1;
    }
    tex[..end].trim_end()
}

/// The passage that shows a manuscript comment. Every manuscript comment gets
/// one while there is any passage, so an edit can never hide a draft.
/// Returns `None` for card comments.
pub fn resolve_passage_comment(comment: &CommentView, passages: &[Passage<'_>]) -> Option<usize> {
    let path = manuscript_comment_path(&comment.card)?;
    let source = comment.source.as_ref();
    let excerpt = source.map(|source| source.text.trim()).filter(|text| !text.is_empty());
    let offset = source.map_or(0, |source| source.range.start as u64);
    let contains = |passage: &Passage<'_>| {
        excerpt.is_some_and(|excerpt| passage.tex.is_some_and(|tex| tex.contains(excerpt)))
    };
    // Passages of the anchored file, nearest first among those with the text.
    let nearest = passages
        .iter()
        .enumerate()
        .filter_map(|(index, passage)| {
            let distance = match passage.text {
                DocProseText::Tex(_) => path.is_empty().then_some(0),
                DocProseText::Source(spans) => spans
                    .iter()
                    .filter(|span| span.path == path)
                    .map(|span| {
                        // The end of a span belongs to what follows it.
                        let last = span.start + (span.len as u64).saturating_sub(1);
                        span.start.saturating_sub(offset).max(offset.saturating_sub(last))
                    })
                    .min(),
            }?;
            Some(((!contains(passage), passage.tex.is_some(), distance), index))
        });
    // An unloaded passage of the file may still hold the text.
    let unloaded = nearest.clone().any(|((_, loaded, _), _)| !loaded);
    let nearest = nearest
        .map(|((missing, _, distance), index)| ((missing, distance), index))
        .min();
    if let Some(((false, _), index)) = nearest {
        return Some(index);
    }
    // The text moved to another file, or the file is gone: follow the text.
    let moved = passages.iter().position(contains).filter(|_| !unloaded);
    moved
        .or_else(|| nearest.map(|(_, index)| index))
        .or_else(|| (!passages.is_empty()).then_some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::DocProseSpan;

    fn span(path: &str, start: u64, tex: &str, newline_before: bool) -> DocProseSpan {
        DocProseSpan {
            path: path.into(),
            start,
            len: tex.len(),
            sha256: String::new(),
            newline_before,
        }
    }

    fn comment(anchor: (String, CommentSource)) -> CommentView {
        CommentView {
            card: anchor.0,
            text: "note".into(),
            quote: None,
            proof: None,
            source: Some(anchor.1),
            created: 0,
        }
    }

    #[test]
    fn abstract_intro_and_prose_comments_follow_their_text_after_a_tex_edit() {
        let sources = [
            ("paper/abstract.tex", "\\begin{abstract}We study fictional spheres.\\end{abstract}"),
            ("paper/main.tex", "Introduction prose about widgets."),
            ("paper/main.tex", "Closing prose about gadgets."),
            ("paper/references.tex", "\\begin{thebibliography}{1}\\bibitem{a} A.\\end{thebibliography}"),
        ];
        let before: Vec<_> = sources
            .iter()
            .zip([0, 10, 400, 0])
            .map(|((path, tex), start)| DocProseText::Source(vec![span(path, start, tex, false)]))
            .collect();
        let comments: Vec<_> = before
            .iter()
            .zip(sources)
            .map(|(text, (_, tex))| comment(passage_comment_anchor(text, tex, None)))
            .collect();
        // Text inserted above moves both main.tex passages past the old offsets.
        let after: Vec<_> = sources
            .iter()
            .zip([0, 395, 900, 0])
            .map(|((path, tex), start)| DocProseText::Source(vec![span(path, start, tex, false)]))
            .collect();
        let loaded: Vec<_> = after
            .iter()
            .zip(sources)
            .map(|(text, (_, tex))| Passage { text, tex: Some(tex) })
            .collect();
        for (index, comment) in comments.iter().enumerate() {
            assert_eq!(resolve_passage_comment(comment, &loaded), Some(index));
        }
        let unloaded: Vec<_> = before.iter().map(|text| Passage { text, tex: None }).collect();
        for (index, comment) in comments.iter().enumerate() {
            assert_eq!(resolve_passage_comment(comment, &unloaded), Some(index));
        }
    }

    #[test]
    fn a_selection_is_anchored_to_the_file_that_contains_it() {
        let (first, second) = ("First file. ", "Second file text.");
        let text = DocProseText::Source(vec![
            span("a.tex", 100, first, false),
            span("b.tex", 20, second, true),
        ]);
        let tex = format!("{first}\n{second}");
        let start = tex.find("file text").unwrap();
        let (card, source) = passage_comment_anchor(&text, &tex, Some(start..start + 4));
        assert_eq!(card, "manuscript:b.tex");
        assert_eq!(source.range, 27..31);
        assert_eq!(source.text, "file");
    }

    #[test]
    fn rewritten_and_renamed_passages_keep_their_comments_visible() {
        let old = DocProseText::Source(vec![span("a.tex", 50, "Old words.", false)]);
        let draft = comment(passage_comment_anchor(&old, "Old words.", None));
        let near = DocProseText::Source(vec![span("a.tex", 60, "New words.", false)]);
        let far = DocProseText::Source(vec![span("a.tex", 900, "Other.", false)]);
        let passages = [
            Passage { text: &far, tex: Some("Other.") },
            Passage { text: &near, tex: Some("New words.") },
        ];
        assert_eq!(resolve_passage_comment(&draft, &passages), Some(1));
        let renamed = DocProseText::Source(vec![span("c.tex", 0, "Old words.", false)]);
        let passages = [
            Passage { text: &far, tex: Some("Other.") },
            Passage { text: &renamed, tex: Some("Old words.") },
        ];
        assert_eq!(resolve_passage_comment(&draft, &passages), Some(1));
        assert_eq!(resolve_passage_comment(&draft, &passages[..1]), Some(0));
        assert_eq!(resolve_passage_comment(&draft, &[]), None);
    }

    #[test]
    fn inline_passages_use_their_text_and_card_comments_are_ignored() {
        let one = DocProseText::Tex("Opening é text.".repeat(40));
        let two = DocProseText::Tex("Closing.".into());
        let draft = comment(passage_comment_anchor(&two, "Closing.", None));
        assert_eq!(draft.card, "manuscript:");
        let DocProseText::Tex(long) = &one else { unreachable!() };
        assert!(passage_comment_anchor(&one, long, None).1.text.len() <= EXCERPT_BYTES);
        let passages = [
            Passage { text: &one, tex: Some(long) },
            Passage { text: &two, tex: Some("Closing.") },
        ];
        assert_eq!(resolve_passage_comment(&draft, &passages), Some(1));
        let card = CommentView { card: "lemma".into(), ..draft };
        assert_eq!(resolve_passage_comment(&card, &passages), None);
    }
}
