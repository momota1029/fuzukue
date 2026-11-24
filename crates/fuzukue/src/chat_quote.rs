const QUOTE_LIMIT: usize = 4000;

/// The edit to insert at the composer caret and the caret's byte offset within it.
pub(super) fn quote_insertion(draft: &str, caret: usize, selection: &str) -> Option<(String, usize)> {
    if selection.trim().is_empty() {
        return None;
    }

    let (before, after) = draft.split_at(caret);
    let mut edit = String::new();
    if !before.is_empty() {
        let trailing_breaks = before.bytes().rev().take_while(|&byte| byte == b'\n').count();
        edit.push_str(&"\n\n"[trailing_breaks.min(2)..]);
    }
    edit.push_str(&format_quote(selection, QUOTE_LIMIT));
    edit.push_str("\n\n");
    let reply = edit.len();
    if !after.is_empty() && !after.starts_with('\n') {
        edit.push('\n');
    }
    Some((edit, reply))
}

fn format_quote(selection: &str, limit: usize) -> String {
    let selection = selection.replace("\r\n", "\n").replace('\r', "\n");
    let selection = selection.trim_matches('\n');
    let lines = || selection.split('\n');
    let length = lines().map(|line| line.chars().count() + 3).sum::<usize>() - 1;
    let mut quote = String::new();
    if length <= limit {
        for line in lines() {
            push_line(&mut quote, line);
        }
        return quote;
    }

    let mut remaining = limit;
    let mut open_fence = None;
    for line in lines() {
        // The selection serializer uses bare tilde fences longer than any
        // tilde run in their contents, and escapes fence markers in prose.
        let fence = (line.len() >= 3 && line.bytes().all(|byte| byte == b'~')).then_some(line);
        let next_fence = match open_fence {
            Some(open) if fence == Some(open) => None,
            Some(open) => Some(open),
            None => fence,
        };
        let overhead = 2 + usize::from(!quote.is_empty());
        let line_length = line.chars().count() + overhead;
        // Always leave room for a quoted ellipsis and any necessary closing fence.
        let reserve = 4 + next_fence.map_or(0, |fence| fence.len() + 3);
        if line_length + reserve > remaining {
            let closing_length = open_fence.map_or(0, |fence: &str| fence.len() + 3);
            let capacity = remaining - overhead - closing_length - 1;
            let mut last: String = if fence.is_some() {
                String::new()
            } else {
                line.chars().take(capacity).collect()
            };
            last.push('…');
            push_line(&mut quote, &last);
            if let Some(fence) = open_fence {
                push_line(&mut quote, fence);
            }
            break;
        }
        push_line(&mut quote, line);
        remaining -= line_length;
        open_fence = next_fence;
    }
    quote
}

fn push_line(quote: &mut String, line: &str) {
    if !quote.is_empty() {
        quote.push('\n');
    }
    quote.push_str("> ");
    quote.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(draft: &str, caret: usize, selected: &str) -> String {
        let (edit, reply) = quote_insertion(draft, caret, selected).unwrap();
        format!("{}{}|{}{}", &draft[..caret], &edit[..reply], &edit[reply..], &draft[caret..])
    }

    #[test]
    fn quotes_every_line_and_leaves_an_empty_reply_line() {
        assert_eq!(insert("", 0, "first\r\n\r\n  second\n"), "> first\n> \n>   second\n\n|");
        assert_eq!(quote_insertion("", 0, " \n"), None);
    }

    #[test]
    fn inserts_at_the_caret_without_losing_the_draft() {
        assert_eq!(insert("前後", "前".len(), "q"), "前\n\n> q\n\n|\n後");
        assert_eq!(insert("a\n\nb", 3, "q"), "a\n\n> q\n\n|\nb");
        assert_eq!(insert("a\nb", 1, "q"), "a\n\n> q\n\n|\nb");
        assert_eq!(insert("a\n", 2, "q"), "a\n\n> q\n\n|");
    }

    #[test]
    fn truncates_by_character_and_keeps_code_fences_balanced() {
        assert_eq!(format_quote(&"あ".repeat(3998), 4000).chars().count(), 4000);
        let truncated = format_quote(&"あ".repeat(3999), 4000);
        assert_eq!(truncated.chars().count(), 4000);
        assert!(truncated.ends_with('…'));
        assert_eq!(format_quote("~~~\none\ntwo\n~~~", 40), "> ~~~\n> one\n> two\n> ~~~");
        assert_eq!(format_quote("~~~~\nvery long code line\n~~~~", 20), "> ~~~~\n> ver…\n> ~~~~");
        assert_eq!(format_quote("~~~~\nvery long code line\n~~~~", 10), "> …");
    }
}
