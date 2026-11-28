//! Track generated lines in an otherwise editable composer.

use std::{collections::HashMap, hash::Hash, ops::Range};

pub struct ComponentDraft<K> {
    text: String,
    slots: HashMap<K, Slot>,
}

struct Slot {
    range: Range<usize>,
    leading_newline: bool,
}

impl<K> Default for ComponentDraft<K> {
    fn default() -> Self {
        Self { text: String::new(), slots: HashMap::new() }
    }
}

impl<K: Eq + Hash> ComponentDraft<K> {
    pub fn preview(&self, key: &K) -> Option<&str> {
        self.slots.get(key).map(|slot| &self.text[slot.range.clone()])
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Rebase intact lines through one editor edit. An edited line becomes
    /// ordinary draft text, safe from subsequent component updates.
    pub fn observe_edit(&mut self, range: Range<usize>, new_end: usize, draft: &str) {
        if self.slots.is_empty() || self.text == draft {
            return;
        }
        if self.text.get(..range.start) != draft.get(..range.start)
            || self.text.get(range.end..) != draft.get(new_end..)
        {
            self.slots.clear();
            return;
        }
        self.rebase(range.clone(), new_end - range.start);
        self.text = draft.to_owned();
        self.slots.retain(|_, slot| {
            let range = &slot.range;
            let starts_line = range.start == 0 || draft.as_bytes()[range.start - 1] == b'\n';
            let ends_line = range.end == draft.len() || draft.as_bytes()[range.end] == b'\n';
            starts_line && ends_line
        });
    }

    /// Return one editor replacement in byte coordinates. Rebase all other
    /// components using the exact replacement before the editor emits Change.
    pub fn update(
        &mut self, key: K, draft: &str, preview: Option<&str>,
    ) -> Option<(Range<usize>, String)> {
        if self.text != draft {
            self.slots.clear();
        }
        let (range, replacement, slot) = match (self.slots.remove(&key), preview) {
            (Some(mut slot), Some(preview)) => {
                let range = slot.range.clone();
                slot.range.end = range.start + preview.len();
                (range, preview.to_owned(), Some(slot))
            }
            (Some(slot), None) => {
                let mut range = slot.range;
                if slot.leading_newline {
                    range.start -= 1;
                } else if draft.as_bytes().get(range.end) == Some(&b'\n') {
                    range.end += 1;
                }
                (range, String::new(), None)
            }
            (None, Some(preview)) => {
                let end = draft.len();
                let leading_newline = end != 0 && !draft.ends_with('\n');
                let replacement = if leading_newline { format!("\n{preview}") } else { preview.to_owned() };
                let slot = Slot {
                    range: end + usize::from(leading_newline)..end + replacement.len(),
                    leading_newline,
                };
                (end..end, replacement, Some(slot))
            }
            (None, None) => return None,
        };
        self.rebase(range.clone(), replacement.len());
        self.text = draft.to_owned();
        self.text.replace_range(range.clone(), &replacement);
        if let Some(slot) = slot {
            self.slots.insert(key, slot);
        }
        (draft[range.clone()] != replacement).then_some((range, replacement))
    }

    fn rebase(&mut self, edit: Range<usize>, new_len: usize) {
        self.slots.retain(|_, slot| {
            let range = &mut slot.range;
            if range.start >= edit.end {
                if range.start == edit.end {
                    slot.leading_newline = false;
                }
                range.start = range.start - edit.len() + new_len;
                range.end = range.end - edit.len() + new_len;
                true
            } else {
                range.end <= edit.start
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(
        slots: &mut ComponentDraft<u8>, draft: &mut String, key: u8, preview: Option<&str>,
    ) {
        if let Some((range, replacement)) = slots.update(key, draft, preview) {
            draft.replace_range(range, &replacement);
        }
    }

    #[test]
    fn updates_rebase_following_slots_and_preserve_user_text() {
        let mut draft = String::from("my draft");
        let mut slots = ComponentDraft::default();
        change(&mut slots, &mut draft, 1, Some("Options: A"));
        change(&mut slots, &mut draft, 2, Some("Count: 3"));
        draft.insert_str(0, "前 ");
        slots.observe_edit(0..0, "前 ".len(), &draft);
        change(&mut slots, &mut draft, 1, Some("Options: A, B"));
        change(&mut slots, &mut draft, 2, Some("Count: 4"));
        assert_eq!(draft, "前 my draft\nOptions: A, B\nCount: 4");
        change(&mut slots, &mut draft, 1, None);
        assert_eq!(draft, "前 my draft\nCount: 4");
    }

    #[test]
    fn a_new_line_at_the_preview_start_keeps_the_slot_attached() {
        let mut draft = String::from("Notes");
        let mut slots = ComponentDraft::default();
        change(&mut slots, &mut draft, 1, Some("Count: 3"));
        draft.insert_str("Notes\n".len(), "Another line\n");
        slots.observe_edit(6..6, "Notes\nAnother line\n".len(), &draft);
        change(&mut slots, &mut draft, 1, Some("Count: 4"));
        assert_eq!(draft, "Notes\nAnother line\nCount: 4");
    }

    #[test]
    fn an_edited_preview_is_never_overwritten_or_removed() {
        let mut draft = String::from("前書き");
        let mut slots = ComponentDraft::default();
        change(&mut slots, &mut draft, 1, Some("Choice: A"));
        let start = draft.len() - 1;
        draft.replace_range(start.., "edited");
        slots.observe_edit(start..start + 1, draft.len(), &draft);
        change(&mut slots, &mut draft, 1, None);
        change(&mut slots, &mut draft, 1, Some("Choice: B"));
        assert_eq!(draft, "前書き\nChoice: edited\nChoice: B");
    }
}
