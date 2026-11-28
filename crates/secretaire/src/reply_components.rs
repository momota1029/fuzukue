//! Connection-local Markdown projection for clients without native reply blocks.

use fuzukue_proto::{Actor, Envelope, MarkdownComponentCapability, ServerEvent};
use fuzukue_view::xml_components::{REPLY_BLOCKS_CAPABILITY, markdown_fallback};
use std::collections::HashMap;

pub(crate) fn supported(capabilities: &[MarkdownComponentCapability]) -> bool {
    capabilities.iter().any(|capability| {
        capability.tag == REPLY_BLOCKS_CAPABILITY && capability.version == 1
    })
}

pub(crate) fn guide(prompt: String, enabled: bool) -> String {
    if !enabled {
        return prompt;
    }
    format!("{prompt}\n\n(secretaire) For this chat reply only, use ordinary Markdown by default. \
        Use a native component sparingly when it makes a comparison, flow or progress easier to read. \
        Put one XML root at column one, with blank lines around it and no fence. \
        Example card: <fuzukue:card title=\"Progress\"><fuzukue:progress value=\"2\" max=\"3\"/></fuzukue:card>. \
        For a flowchart, use <fuzukue:flowchart> containing <fuzukue:step label=\"Review\"/> \
        and <fuzukue:decision question=\"Approved?\"> with at least two \
        <fuzukue:branch label=\"Yes\">Publish</fuzukue:branch> outcomes; close every open tag. \
        XML-escape & and < in content. Use this syntax only in the direct chat reply, \
        never in files, delegated instructions, GitHub, email, or other external output.")
}

#[derive(Default)]
struct Reply {
    source: String,
    sent: usize,
}

// The cursor is local to the connection; the journal retains the original
// deltas so replay can reconstruct the same projection at the resume point.
#[derive(Default)]
pub(crate) struct Fallback {
    sessions: HashMap<String, Vec<(String, Actor, Reply)>>,
}

impl Fallback {
    pub(crate) fn stream_event(event: &ServerEvent) -> bool {
        matches!(event, ServerEvent::TextDelta { actor, item, .. } if partner(actor, item))
            || matches!(event, ServerEvent::TurnEnded { actor, .. } if crate::routing::is_partner(actor))
            || matches!(event, ServerEvent::ReplyFinalized { .. } | ServerEvent::ReplySuperseded { .. })
    }

    pub(crate) fn relevant(event: &ServerEvent) -> bool {
        Self::stream_event(event) || matches!(event,
            ServerEvent::ThreadRestored { .. }
                | ServerEvent::ThreadHistoryPage { .. }
                | ServerEvent::ThreadTail { .. }
                | ServerEvent::ThreadHistoryFrom { .. })
    }

    pub(crate) fn apply(
        &mut self,
        envelope: &mut Envelope,
        running: impl Fn(&str) -> bool,
    ) -> Vec<Envelope> {
        let current_tail = matches!(&envelope.event,
            ServerEvent::ThreadRestored { .. } | ServerEvent::ThreadTail { .. }
                | ServerEvent::ThreadHistoryFrom { next_after: None, .. });
        match &mut envelope.event {
            ServerEvent::TextDelta { session, item, actor, text } if partner(actor, item) => {
                let replies = self.sessions.entry(session.clone()).or_default();
                let index = match replies.iter().position(|(id, _, _)| id == item) {
                    Some(index) => index,
                    None => {
                        replies.push((item.clone(), actor.clone(), Reply::default()));
                        replies.len() - 1
                    }
                };
                let reply = &mut replies[index].2;
                reply.source.push_str(text);
                *text = reply.take_delta(true);
            }
            ServerEvent::ReplyFinalized { session, item, actor } => {
                let reply = self.sessions.get_mut(session).and_then(|items| {
                    let index = items.iter().position(|(id, _, _)| id == item)?;
                    Some(items.remove(index).2)
                });
                let text = reply.map(|mut reply| reply.take_delta(false)).unwrap_or_default();
                envelope.event = ServerEvent::TextDelta {
                    session: session.clone(), item: item.clone(), actor: actor.clone(), text,
                };
            }
            ServerEvent::TurnEnded { session, actor } if crate::routing::is_partner(actor) => {
                return self.sessions.remove(session).unwrap_or_default()
                    .into_iter()
                    .filter_map(|(item, actor, mut reply)| {
                        let text = reply.take_delta(false);
                        if text.is_empty() {
                            return None;
                        }
                        Some(crate::snapshot(ServerEvent::TextDelta {
                            session: session.clone(), item, actor, text,
                        }))
                    })
                    .collect();
            }
            ServerEvent::ThreadRestored { events, .. }
            | ServerEvent::ThreadHistoryPage { events, .. }
            | ServerEvent::ThreadTail { events, .. }
            | ServerEvent::ThreadHistoryFrom { events, .. } => {
                // History coalesces each message to one event. A row followed
                // by TurnEnded is final. The suffix can still be streaming
                // when a connection requests its transcript.
                let mut open = HashMap::new();
                for (i, event) in events.iter().enumerate() {
                    match event {
                        ServerEvent::TextDelta { session, item, actor, .. } if partner(actor, item) => {
                            open.insert((session.as_str(), item.as_str()), i);
                        }
                        ServerEvent::TurnEnded { session, actor } if crate::routing::is_partner(actor) => {
                            open.retain(|(owner, _), _| *owner != session);
                        }
                        _ => {}
                    }
                }
                let open: std::collections::HashSet<_> = open.into_values().collect();
                for (i, event) in events.iter_mut().enumerate() {
                    if let ServerEvent::TextDelta { session, item, actor, text } = event {
                        if !partner(actor, item) {
                            continue;
                        }
                        let active = current_tail && open.contains(&i) && running(session);
                        let projected = markdown_fallback(text, active);
                        if active {
                            let items = self.sessions.entry(session.clone()).or_default();
                            let reply = Reply { source: std::mem::take(text), sent: projected.len() };
                            let existing = items.iter_mut().find(|(id, _, _)| id == item);
                            match existing {
                                Some((_, _, existing)) if reply.source.starts_with(&existing.source) => {
                                    *existing = reply;
                                }
                                None => items.push((item.clone(), actor.clone(), reply)),
                                _ => {}
                            }
                        }
                        *text = projected;
                    }
                }
            }
            ServerEvent::ReplySuperseded { session, items } => {
                if let Some(replies) = self.sessions.get_mut(session) {
                    replies.retain(|(item, _, _)| !items.contains(item));
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

impl Reply {
    fn take_delta(&mut self, streaming: bool) -> String {
        let projected = markdown_fallback(&self.source, streaming);
        let delta = projected.get(self.sent..).unwrap_or_default().to_owned();
        self.sent = self.sent.max(projected.len());
        delta
    }
}

fn partner(actor: &Actor, item: &str) -> bool {
    crate::routing::is_partner(actor) && !item.starts_with(fuzukue_proto::DELEGATE_JOB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> Envelope {
        crate::snapshot(ServerEvent::TextDelta {
            session: "thread-example".into(),
            item: "reply".into(),
            actor: Actor::Claude,
            text: text.into(),
        })
    }

    #[test]
    fn resume_and_tail_snapshot_keep_only_the_projection_cursor() {
        let prefix = "Before\n\n<fuzukue:flowchart>\n";
        let suffix = "  <fuzukue:step label=\"Review\"/>\n</fuzukue:flowchart>";
        let hub = crate::hub::Hub::new();
        hub.publish(delta(prefix).event);
        let (initial, _) = hub.subscribe(0);
        let cursor = initial.iter().filter(|event| event.seq > 0).map(|event| event.seq).max().unwrap();
        let mut resumed = hub.reply_fallback_at(cursor);
        let mut attached = Fallback::default();
        let mut history = crate::snapshot(ServerEvent::ThreadRestored {
            session: "thread-example".into(),
            events: vec![delta(prefix).event],
        });
        attached.apply(&mut history, |_| true);
        for fallback in [&mut resumed, &mut attached] {
            let mut closing = delta(suffix);
            fallback.apply(&mut closing, |_| true);
            assert!(matches!(closing.event, ServerEvent::TextDelta { text, .. } if text.is_empty()));
            let mut finalized = Envelope {
                workspace: None,
                seq: cursor + 2,
                event: ServerEvent::ReplyFinalized {
                    session: "thread-example".into(), item: "reply".into(), actor: Actor::Claude,
                },
            };
            assert!(fallback.apply(&mut finalized, |_| true).is_empty());
            assert_eq!(finalized.seq, cursor + 2);
            assert!(matches!(finalized.event, ServerEvent::TextDelta { text, .. } if text.contains("↓ Review")));
            let mut ended = crate::snapshot(ServerEvent::TurnEnded {
                session: "thread-example".into(), actor: Actor::Claude,
            });
            assert!(fallback.apply(&mut ended, |_| false).is_empty());
        }
    }

    #[test]
    fn legacy_client_gets_stable_deltas_and_same_history() {
        let source = "Intro\n\n<fuzukue:card title=\"Review\">\n<fuzukue:text>Done</fuzukue:text>\n</fuzukue:card>\n\nAfter";
        let mut fallback = Fallback::default();
        let mut delivered = String::new();
        for ch in source.chars() {
            let mut event = delta(&ch.to_string());
            assert!(fallback.apply(&mut event, |_| false).is_empty());
            let ServerEvent::TextDelta { text, .. } = event.event else { unreachable!() };
            delivered.push_str(&text);
            assert!(!delivered.contains("<fuzukue"));
        }
        assert_eq!(delivered, markdown_fallback(source, false));
        let mut history = crate::snapshot(ServerEvent::ThreadRestored {
            session: "thread-example".into(),
            events: vec![delta(source).event],
        });
        Fallback::default().apply(&mut history, |_| false);
        let ServerEvent::ThreadRestored { events, .. } = history.event else { unreachable!() };
        let ServerEvent::TextDelta { text, .. } = &events[0] else { unreachable!() };
        assert_eq!(*text, delivered);
    }
}
