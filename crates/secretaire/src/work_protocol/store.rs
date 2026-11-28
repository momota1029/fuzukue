//! Durable protocol state for one configured route and one local writer.

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use super::{wire, Ack, Event, GitRepo, Oid, Payload, PublishPlan, Route, Stream};

pub(super) const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
const PUBLISH_BATCH: usize = 32;

/// The result of a successful capture. A failed fetch is not a capture.
#[derive(Debug)]
pub enum Capture {
    Present(Oid),
    Missing,
}

impl Capture {
    fn tip(&self) -> Option<&Oid> {
        match self {
            Self::Present(tip) => Some(tip),
            Self::Missing => None,
        }
    }
}

/// The single record describing why a stream stopped advancing. The capture
/// layer must keep both OIDs reachable while this record is open.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Quarantine {
    pub accepted_tip: Option<String>,
    pub captured_tip: Option<String>,
    pub commit: Option<String>,
    pub next_seq: u64,
    pub reason: String,
}

/// The outcome of processing one captured stream tip. Deferred observations
/// can be retried after reconciling publication of the local stream.
#[derive(Default, Debug)]
pub struct Observation {
    pub advanced: usize,
    pub deferred: bool,
    pub quarantine: Option<Quarantine>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub event: Event,
    pub sha256: String,
    pub commit: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Cursor {
    observed: bool,
    tip: Option<String>,
    count: usize,
    quarantine: Option<Quarantine>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Queued {
    operation_id: Option<String>,
    event: Event,
    raw: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct State {
    route: Route,
    writer: Stream,
    incoming: Cursor,
    outgoing: Cursor,
    inbox: Vec<Receipt>,
    outbox: Vec<Queued>,
    incoming_bytes: usize,
    outgoing_bytes: usize,
    delivered_through: u64,
    acknowledged_through: u64,
}

/// Holds the exclusive writer lock for a caller-owned durable state file.
/// Use one stable path per communication identity, run, and local writer.
pub struct Session {
    path: PathBuf,
    state: State,
    poisoned: bool,
    lock: File,
}

impl Drop for Session {
    fn drop(&mut self) {
        // A concurrent Git spawn can briefly inherit the file description
        // before close-on-exec. Release the lock even if that copy still exists.
        let _ = self.lock.unlock();
    }
}

impl Session {
    pub fn open(path: impl Into<PathBuf>, route: Route, writer: Stream) -> Result<Self> {
        route.validate()?;
        let path = path.into();
        let parent = path.parent().filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        crate::durable::create_dir_all(parent)?;
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(lock_path)?;
        lock.try_lock().context("protocol state is already locked or cannot be locked")?;
        let state: State = match File::open(&path) {
            Ok(file) => serde_json::from_reader(BufReader::new(file))
                .context("read durable protocol state")?,
            Err(error) if error.kind() == ErrorKind::NotFound => State {
                route: route.clone(),
                writer,
                incoming: Cursor::default(),
                outgoing: Cursor::default(),
                inbox: Vec::new(),
                outbox: Vec::new(),
                incoming_bytes: 0,
                outgoing_bytes: 0,
                delivered_through: 0,
                acknowledged_through: 0,
            },
            Err(error) => return Err(error.into()),
        };
        ensure!(
            state.route == route && state.writer == writer,
            "protocol state belongs to a different route or writer"
        );
        ensure!(
            state.incoming.count == state.inbox.len()
                && state.outgoing.count <= state.outbox.len()
                && state.delivered_through <= state.incoming.count as u64
                && state.acknowledged_through <= state.outgoing.count as u64
                && state.incoming_bytes <= MAX_STREAM_BYTES
                && state.outgoing_bytes <= MAX_STREAM_BYTES,
            "protocol state has inconsistent cursors"
        );
        Ok(Self { path, state, poisoned: false, lock })
    }

    /// Reconcile the local stream before reading the peer. This also resolves
    /// any successful publication whose response or local checkpoint was lost.
    pub fn observe(
        &mut self,
        git: &GitRepo,
        stream: Stream,
        capture: &Capture,
        observed_at: DateTime<Utc>,
    ) -> Result<Observation> {
        self.check_writable()?;
        let cursor = self.state.cursor(stream);
        if let Some(issue) = &cursor.quarantine {
            return Ok(Observation { quarantine: Some(issue.clone()), ..Observation::default() });
        }
        let previous = cursor.tip.as_deref().map(Oid::parse).transpose()?;
        let tip = capture.tip();
        if cursor.observed && previous.as_ref() == tip {
            return Ok(Observation::default());
        }

        let mut next = self.state.clone();
        next.cursor_mut(stream).observed = true;
        let mut outcome = Observation::default();
        let history = match tip {
            Some(tip) => git.history(previous.as_ref(), tip),
            None if previous.is_none() => Ok(Vec::new()),
            None => Err(anyhow::anyhow!("previously observed stream was deleted")),
        };
        match history {
            Err(error) => next.quarantine(stream, tip, tip, error),
            Ok(history) => {
                let mut parent = previous;
                for oid in history {
                    let result = git.read_append(&next.route, stream, &oid, parent.as_ref())
                        .and_then(|events| next.accept(stream, &oid, events, observed_at));
                    match result {
                        Ok(Some(count)) => {
                            outcome.advanced += count;
                            next.cursor_mut(stream).tip = Some(oid.as_str().into());
                            parent = Some(oid);
                        }
                        Ok(None) => {
                            outcome.deferred = true;
                            break;
                        }
                        Err(error) => {
                            next.quarantine(stream, tip, Some(&oid), error);
                            break;
                        }
                    }
                }
            }
        }
        outcome.quarantine = next.cursor(stream).quarantine.clone();
        if outcome.advanced > 0 || outcome.quarantine.is_some() || !cursor.observed {
            self.save(next)?;
        }
        Ok(outcome)
    }

    pub fn accepted_tip(&self, stream: Stream) -> Result<Option<Oid>> {
        self.state.cursor(stream).tip.as_deref().map(Oid::parse).transpose()
    }

    /// The trusted route bound to this durable session.
    pub fn route(&self) -> &Route {
        &self.state.route
    }

    /// Retained application events that have not been durably delivered to
    /// the caller. Protocol acknowledgments are handled internally.
    pub fn unread(&self) -> impl Iterator<Item = &Receipt> {
        self.state.inbox.iter().filter(|receipt| {
            receipt.event.seq > self.state.delivered_through && !receipt.event.payload.is_ack()
        })
    }

    /// Call after the downstream consumer has durably stored this event ID.
    /// Confirming the same ID again is safe. Gaps cannot be acknowledged.
    pub fn mark_delivered(&mut self, event_id: &str) -> Result<()> {
        self.check_writable()?;
        let seq = self.state.route.sequence(event_id, self.state.writer.peer())?;
        if seq <= self.state.delivered_through {
            return Ok(());
        }
        ensure!(
            self.unread().next().is_some_and(|receipt| receipt.event.seq == seq),
            "delivery confirmation is out of order or unknown"
        );
        let mut next = self.state.clone();
        next.delivered_through = seq;
        self.save(next)
    }

    /// Queue a local action under a stable operation ID, such as the ID of a
    /// saved user answer. A retry returns the original event with its timestamp
    /// and sequence. Changing the payload requires a new operation ID.
    pub fn queue(
        &mut self,
        operation_id: &str,
        at: DateTime<Utc>,
        payload: Payload,
    ) -> Result<Event> {
        self.check_writable()?;
        ensure!(
            !operation_id.is_empty() && operation_id.len() <= 256
                && !operation_id.chars().any(char::is_control),
            "invalid local operation ID"
        );
        ensure!(!payload.is_ack(), "use queue_ack for protocol acknowledgments");
        if let Some(queued) = self.state.outbox.iter()
            .find(|queued| queued.operation_id.as_deref() == Some(operation_id))
        {
            ensure!(queued.event.payload == payload, "operation ID already has a different payload");
            return Ok(queued.event.clone());
        }
        self.enqueue(Some(operation_id.into()), at, payload)
    }

    /// Queue a cumulative acknowledgment after receipt is durable. Call each
    /// poll to put an upper bound on the delay, even with no other work to send.
    /// Incoming ack-only traffic does not produce another ack.
    pub fn queue_ack(&mut self, at: DateTime<Utc>) -> Result<Option<Event>> {
        self.check_writable()?;
        let Some(latest) = self.state.inbox.iter().rev().find(|item| !item.event.payload.is_ack()) else {
            return Ok(None);
        };
        let already_queued = self.state.outbox.iter().rev().find_map(|item| match &item.event.payload {
            Payload::Ack(ack) => Some(ack.through),
            _ => None,
        }).unwrap_or(0);
        if already_queued >= latest.event.seq {
            return Ok(None);
        }
        let ack = Ack { through: latest.event.seq, event_id: latest.event.id.clone() };
        self.enqueue(None, at, Payload::Ack(ack)).map(Some)
    }

    /// Prepare the next batch using the last reconciled local OID. Approval
    /// must cover the exact plan, including its Git metadata and destination.
    pub fn prepare_publish(&self, git: &GitRepo) -> Result<Option<PublishPlan>> {
        self.check_writable()?;
        let state = &self.state;
        ensure!(state.outgoing.quarantine.is_none(), "local stream is quarantined");
        let batch: Vec<_> = state.outbox[state.outgoing.count..].iter().take(PUBLISH_BATCH)
            .map(|entry| (entry.event.clone(), entry.raw.as_str())).collect();
        if batch.is_empty() {
            return Ok(None);
        }
        ensure!(state.outgoing.observed, "capture the local stream before publishing");
        let previous = state.outgoing.tip.as_deref().map(Oid::parse).transpose()?;
        let tip = git.build_commit(&state.route, state.writer, previous.as_ref(), &batch)?;
        Ok(Some(PublishPlan {
            route: state.route.clone(),
            stream: state.writer,
            previous,
            tip,
            events: batch.into_iter().map(|(event, _)| event).collect(),
        }))
    }

    /// Publish an approved plan against its exact previous tip. The outbox
    /// advances only when a later capture confirms the published bytes.
    pub fn publish(
        &self,
        git: &GitRepo,
        plan: &PublishPlan,
        approve: impl FnOnce(&PublishPlan) -> Result<()>,
    ) -> Result<()> {
        self.check_writable()?;
        let state = &self.state;
        ensure!(state.outgoing.quarantine.is_none(), "local stream is quarantined");
        ensure!(
            plan.route == state.route && plan.stream == state.writer
                && plan.previous().map(Oid::as_str) == state.outgoing.tip.as_deref(),
            "publication plan no longer matches the local stream"
        );
        let pending = &state.outbox[state.outgoing.count..];
        ensure!(
            plan.events.len() <= pending.len()
                && pending.iter().zip(&plan.events).all(|(item, event)| &item.event == event),
            "publication plan no longer matches the outbox"
        );
        git.publish(plan, approve)
    }

    pub fn published_through(&self) -> u64 {
        self.state.outgoing.count as u64
    }

    pub fn acknowledged_through(&self) -> u64 {
        self.state.acknowledged_through
    }

    /// Local application events still waiting for a peer receipt, including
    /// events that have yet to be published.
    pub fn outstanding(&self) -> impl Iterator<Item = &Event> {
        self.state.outbox.iter().map(|queued| &queued.event).filter(|event| {
            event.seq > self.state.acknowledged_through && !event.payload.is_ack()
        })
    }

    /// Desk-side decision records with no reply queued yet. A queued reply
    /// and the original question stay in the same durable state; use the
    /// published and acknowledged positions to show its delivery status.
    pub fn pending_decisions(&self) -> impl Iterator<Item = &Receipt> {
        self.state.inbox.iter().filter(|item| {
            self.state.writer == Stream::Desk
                && matches!(item.event.payload, Payload::DecisionRequest(_))
                && self.state.latest_reply(Stream::Desk, &item.event.id).is_none()
        })
    }

    fn enqueue(
        &mut self,
        operation_id: Option<String>,
        at: DateTime<Utc>,
        payload: Payload,
    ) -> Result<Event> {
        let state = &self.state;
        ensure!(state.outgoing.quarantine.is_none(), "local stream is quarantined");
        ensure!(state.outbox.len() < wire::MAX_EVENTS, "local stream reached its event limit");
        let seq = state.outbox.len() as u64 + 1;
        let event = Event::new(&state.route, state.writer, seq, at, payload);
        let raw = serde_json::to_string(&event)?;
        let event = Event::parse(&raw, &state.route, state.writer, seq)?;
        ensure!(
            state.outgoing_bytes + raw.len() <= MAX_STREAM_BYTES,
            "local stream reached its byte limit"
        );
        ensure!(state.validate_links(&event, true)?, "event dependency is not available");
        let mut next = state.clone();
        next.outgoing_bytes += raw.len();
        next.outbox.push(Queued { operation_id, event: event.clone(), raw });
        self.save(next)?;
        Ok(event)
    }

    fn check_writable(&self) -> Result<()> {
        ensure!(!self.poisoned, "reopen protocol state after persistence failure");
        Ok(())
    }

    fn save(&mut self, next: State) -> Result<()> {
        // A failed directory sync can leave the replacement visible. Require a
        // reload before using this session again so a retry cannot overwrite it.
        self.poisoned = true;
        crate::durable::json(&self.path, &next)?;
        self.state = next;
        self.poisoned = false;
        Ok(())
    }
}

impl State {
    fn cursor(&self, stream: Stream) -> &Cursor {
        if self.writer == stream { &self.outgoing } else { &self.incoming }
    }

    fn cursor_mut(&mut self, stream: Stream) -> &mut Cursor {
        if self.writer == stream { &mut self.outgoing } else { &mut self.incoming }
    }

    fn quarantine(
        &mut self,
        stream: Stream,
        tip: Option<&Oid>,
        commit: Option<&Oid>,
        error: anyhow::Error,
    ) {
        let cursor = self.cursor_mut(stream);
        cursor.quarantine = Some(Quarantine {
            accepted_tip: cursor.tip.clone(),
            captured_tip: tip.map(|oid| oid.as_str().into()),
            commit: commit.map(|oid| oid.as_str().into()),
            next_seq: cursor.count as u64 + 1,
            reason: format!("{error:#}").chars().take(512).collect(),
        });
    }

    fn accept(
        &mut self,
        stream: Stream,
        commit: &Oid,
        records: Vec<(u64, String)>,
        observed_at: DateTime<Utc>,
    ) -> Result<Option<usize>> {
        let start = self.cursor(stream).count;
        let count = records.len();
        ensure!(start + count <= wire::MAX_EVENTS, "stream reached its event limit");
        for (index, (seq, _)) in records.iter().enumerate() {
            ensure!(*seq == (start + index + 1) as u64, "sequence gap at {}", start + index + 1);
        }
        if stream == self.writer {
            for (seq, raw) in records {
                ensure!(
                    self.outbox.get(seq as usize - 1).is_some_and(|item| item.raw == raw),
                    "published sequence {seq} does not match the local outbox"
                );
            }
            self.outgoing.count += count;
            return Ok(Some(count));
        }

        let initial_bytes = self.incoming_bytes;
        let initial_ack = self.acknowledged_through;
        let result = (|| {
            for (seq, raw) in records {
                let event = Event::parse(&raw, &self.route, stream, seq)
                    .with_context(|| format!("invalid sequence {seq}"))?;
                ensure!(self.incoming_bytes + raw.len() <= MAX_STREAM_BYTES, "stream reached its byte limit");
                if !self.validate_links(&event, false)? {
                    return Ok(None);
                }
                if let Payload::Ack(ack) = &event.payload {
                    self.acknowledged_through = self.acknowledged_through.max(ack.through);
                }
                self.incoming_bytes += raw.len();
                self.inbox.push(Receipt {
                    event,
                    sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
                    commit: commit.as_str().into(),
                    observed_at,
                });
            }
            Ok(Some(count))
        })();
        if matches!(&result, Ok(Some(_))) {
            self.incoming.count += count;
        } else {
            self.inbox.truncate(start);
            self.incoming_bytes = initial_bytes;
            self.acknowledged_through = initial_ack;
        }
        result
    }

    // A locally known dependency may have reached the other writer just
    // after our local-stream capture. Retry after the next reconciliation.
    fn validate_links(&self, event: &Event, outgoing: bool) -> Result<bool> {
        let stream = event.stream;
        let peer = stream.peer();
        match &event.payload {
            Payload::Ack(ack) => {
                let seq = self.route.sequence(&ack.event_id, peer)?;
                ensure!(seq == ack.through, "acknowledgment position and ID disagree");
                let target = self.link(&ack.event_id, peer)?;
                ensure!(!target.payload.is_ack(), "an ack cannot target another ack");
                if !outgoing && seq > self.outgoing.count as u64 {
                    return Ok(false);
                }
            }
            Payload::DecisionRequest(request) => {
                if let Some(id) = &request.context_event {
                    let context_stream = if self.route.sequence(id, stream).is_ok() { stream } else { peer };
                    let target = self.link(id, context_stream)?;
                    if context_stream == stream {
                        ensure!(target.seq < event.seq, "decision context must precede the request");
                    } else if !outgoing && target.seq > self.outgoing.count as u64 {
                        return Ok(false);
                    }
                }
            }
            Payload::DecisionReply(reply) => {
                let target = self.link(&reply.request_id, peer)?;
                let Payload::DecisionRequest(request) = &target.payload else {
                    bail!("reply does not reference a decision request");
                };
                ensure!(
                    request.options.iter().any(|option| option.id == reply.option_id),
                    "reply selects an unknown option"
                );
                ensure!(
                    self.latest_reply(stream, &reply.request_id).map(|event| event.id.as_str())
                        == reply.corrects.as_deref(),
                    "reply must correct the latest answer to this request"
                );
                if !outgoing && target.seq > self.outgoing.count as u64 {
                    return Ok(false);
                }
            }
            Payload::Complete(complete) => {
                let target = self.link(&complete.report_id, stream)?;
                ensure!(
                    target.seq < event.seq && matches!(target.payload, Payload::Report(_)),
                    "completion must reference a preceding report"
                );
            }
            Payload::Instruction(instruction) => {
                self.check_assignment(&instruction.assignment_id, event)?;
            }
            Payload::Stop(stop) => self.check_assignment(&stop.assignment_id, event)?,
            _ => {}
        }
        Ok(true)
    }

    fn check_assignment(&self, id: &str, event: &Event) -> Result<()> {
        let target = self.link(id, Stream::Desk)?;
        ensure!(
            target.seq < event.seq && matches!(target.payload, Payload::Assignment(_)),
            "instruction or stop must reference a preceding assignment"
        );
        Ok(())
    }

    fn link(&self, id: &str, stream: Stream) -> Result<&Event> {
        let seq = self.route.sequence(id, stream)?;
        ensure!(seq <= wire::MAX_EVENTS as u64, "event reference exceeds stream limit");
        let index = seq as usize - 1;
        if stream == self.writer {
            self.outbox.get(index).map(|item| &item.event)
        } else {
            self.inbox.get(index).map(|item| &item.event)
        }.context("event references an unknown sequence")
    }

    fn latest_reply(&self, stream: Stream, request_id: &str) -> Option<&Event> {
        let mut events: Box<dyn Iterator<Item = &Event> + '_> = if stream == self.writer {
            Box::new(self.outbox.iter().rev().map(|item| &item.event))
        } else {
            Box::new(self.inbox.iter().rev().map(|item| &item.event))
        };
        events.find(|event| {
            matches!(&event.payload, Payload::DecisionReply(reply) if reply.request_id == request_id)
        })
    }
}
