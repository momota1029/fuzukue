use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use super::{
    draft::msgids, ensure_review_allowed, imap, message_key, mime, wrap, ExaminedMailbox, Filter,
    Folder, Header, HeaderFetch, ImapSource, ImapTransport, RawMessage, Uid, MAX_CHARS,
};

const MAX_MESSAGES: usize = 50;
const MAX_DEPTH: usize = 4;
const MAX_IDS: usize = 64;
const IDS_PER_SEARCH: usize = 8;
const CANDIDATES_PER_FOLDER: usize = 100;

pub(super) struct Thread {
    pub(super) data: Value,
    pub(super) validities: BTreeMap<String, u32>,
}

struct Member {
    folder: String,
    uid: Uid,
    seen: bool,
    fields: mime::Headers,
}

impl Member {
    fn new(folder: &str, header: Header) -> Result<Self> {
        Ok(Self {
            folder: folder.into(),
            uid: header.uid,
            seen: header.seen,
            fields: mime::Headers::parse(&header.bytes)?,
        })
    }

    fn links(&self) -> impl Iterator<Item = String> {
        msgids(&self.fields.message_id)
            .into_iter()
            .chain(msgids(&self.fields.in_reply_to))
            .chain(msgids(&self.fields.references))
    }
}

// The walk accepts only exact parsed IDs, since IMAP HEADER matches substrings.
// Track physical locations even when Gmail labels or archives duplicate a message.
struct Walk {
    members: Vec<Member>,
    examined: BTreeMap<String, BTreeSet<Uid>>,
    displayed: BTreeSet<String>,
    known_ids: BTreeSet<String>,
    pending: Vec<String>,
    truncated: bool,
}

impl Walk {
    fn new(root: Member) -> Self {
        let mut walk = Self {
            members: Vec::new(),
            examined: BTreeMap::new(),
            displayed: BTreeSet::new(),
            known_ids: BTreeSet::new(),
            pending: Vec::new(),
            truncated: false,
        };
        walk.add(root);
        walk
    }

    fn add(&mut self, member: Member) {
        self.examined
            .entry(member.folder.clone())
            .or_default()
            .insert(member.uid);
        let mid = msgids(&member.fields.message_id).into_iter().next();
        let display = mid.as_ref().is_none_or(|id| !self.displayed.contains(id));
        if display && self.members.len() == MAX_MESSAGES {
            self.truncated = true;
            return;
        }
        for id in member.links() {
            if self.known_ids.contains(&id) {
                continue;
            }
            if self.known_ids.len() == MAX_IDS {
                self.truncated = true;
                break;
            }
            self.known_ids.insert(id.clone());
            self.pending.push(id);
        }
        if display {
            self.displayed.extend(mid);
            self.members.push(member);
        }
    }

    fn consider(&mut self, member: Member, frontier: &BTreeSet<&str>) {
        if member.links().any(|id| frontier.contains(id.as_str())) {
            self.add(member);
        }
    }
}

impl<T: ImapTransport> ImapSource<T> {
    pub(super) async fn thread(
        &mut self,
        account: &str,
        selected: &str,
        uid: Uid,
        validity: u32,
        review: &Regex,
    ) -> Result<Thread> {
        let wire = imap::encode_folder(selected)?;
        let root = {
            let mut mailbox = examine(&mut self.0, &wire, Some(validity)).await?;
            let header = mailbox
                .peek_headers(&[uid], HeaderFetch::Thread)
                .await?
                .into_iter()
                .find(|header| header.uid == uid)
                .context("指定されたメールが見つからない")?;
            Member::new(selected, header)?
        };

        let mut walk = Walk::new(root);
        let mut validities = BTreeMap::from([(selected.to_owned(), validity)]);
        let mut folders = Vec::new();
        if !walk.pending.is_empty() {
            folders = self.0.list().await?;
            folders.retain(|folder| folder.name != selected);
            folders.sort_by_key(|folder| {
                !folder
                    .special_use
                    .iter()
                    .any(|flag| matches!(flag.as_str(), "\\Sent" | "\\All" | "\\Archive"))
            });
        }
        folders.insert(
            0,
            Folder {
                name: selected.into(),
                wire,
                special_use: Vec::new(),
            },
        );

        for _ in 0..MAX_DEPTH {
            let frontier = std::mem::take(&mut walk.pending);
            if frontier.is_empty() {
                break;
            }
            let queries: Vec<_> = frontier
                .chunks(IDS_PER_SEARCH)
                .map(|ids| Filter::ThreadIds(ids.to_vec()))
                .collect();
            let frontier = frontier.iter().map(String::as_str).collect();
            for folder in &folders {
                let mut mailbox = examine(
                    &mut self.0,
                    &folder.wire,
                    validities.get(&folder.name).copied(),
                )
                .await?;
                validities.insert(folder.name.clone(), mailbox.uid_validity());
                let mut uids = BTreeSet::new();
                for query in &queries {
                    uids.extend(mailbox.uid_search(std::slice::from_ref(query)).await?);
                }
                let examined = walk.examined.get(&folder.name);
                let mut candidates: Vec<_> = uids
                    .into_iter()
                    .rev()
                    .filter(|uid| examined.is_none_or(|examined| !examined.contains(uid)))
                    .collect();
                walk.truncated |= candidates.len() > CANDIDATES_PER_FOLDER;
                candidates.truncate(CANDIDATES_PER_FOLDER);
                if !candidates.is_empty() {
                    for header in mailbox
                        .peek_headers(&candidates, HeaderFetch::Thread)
                        .await?
                    {
                        walk.consider(Member::new(&folder.name, header)?, &frontier);
                    }
                }
            }
        }

        let truncated = walk.truncated || !walk.pending.is_empty();
        let mut messages = Vec::new();
        walk.members
            .sort_by(|a, b| a.folder.cmp(&b.folder).then(a.uid.cmp(&b.uid)));
        for group in walk.members.chunk_by(|a, b| a.folder == b.folder) {
            let name = &group[0].folder;
            let folder = folders.iter().find(|folder| &folder.name == name).unwrap();
            let validity = validities[name];
            let mut mailbox = if group
                .iter()
                .any(|member| !review.is_match(&member.fields.subject))
            {
                Some(examine(&mut self.0, &folder.wire, Some(validity)).await?)
            } else {
                None
            };
            for member in group {
                let id = format!("{account}:{name}:{}", member.uid);
                let key = message_key(&id, validity);
                let denied = review.is_match(&member.fields.subject);
                let mut value = if denied {
                    mime::withheld(&id, &key, &member.fields, member.seen)
                } else {
                    let bytes = mailbox.as_mut().unwrap().peek_message(member.uid).await?;
                    let fields = mime::Headers::parse(&bytes)?;
                    ensure_review_allowed(review, &fields.subject)?;
                    ensure!(
                        fields.subject == member.fields.subject
                            && fields.message_id == member.fields.message_id,
                        "取得中にメールが変わった。検索し直す"
                    );
                    let raw = RawMessage {
                        id,
                        key: key.clone(),
                        bytes,
                        seen: member.seen,
                    };
                    tokio::task::spawn_blocking(move || mime::read(&raw, MAX_CHARS)).await??
                };
                value["folder"] = json!(wrap(&key, name));
                messages.push((member.fields.when(), value));
            }
        }
        messages.sort_by_key(|(date, _)| (date.is_none(), *date));
        let messages = messages
            .into_iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>();
        validities.retain(|folder, _| walk.members.iter().any(|member| &member.folder == folder));
        Ok(Thread {
            data: json!({ "messages": messages, "truncated": truncated }),
            validities,
        })
    }
}

async fn examine<'a, T: ImapTransport>(
    transport: &'a mut T,
    wire: &str,
    expected: Option<u32>,
) -> Result<T::Mailbox<'a>> {
    let mailbox = transport.examine(wire).await?;
    ensure!(
        expected.is_none_or(|expected| mailbox.uid_validity() == expected),
        "IMAP フォルダーが更新された。メールを検索し直す"
    );
    Ok(mailbox)
}
