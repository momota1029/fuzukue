//! The v1 wire format. Parse the envelope and each concrete payload separately
//! so duplicate and unknown fields are rejected at every object level.

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;

pub const MAX_HEADER_BYTES: usize = 4 * 1024;
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_EVENTS: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Desk,
    Worker,
}

impl Stream {
    pub fn peer(self) -> Self {
        match self {
            Self::Desk => Self::Worker,
            Self::Worker => Self::Desk,
        }
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Desk => "desk",
            Self::Worker => "worker",
        })
    }
}

/// A route provisioned by the caller's trusted configuration.
///
/// The destination must identify one authorized Git endpoint. Give a changed
/// endpoint a new communication identity. Scope and run are opaque IDs.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    communication: String,
    destination: String,
    scope: String,
    run: String,
    private: bool,
}

impl Route {
    pub fn new(
        communication: &str,
        destination: &str,
        scope: &str,
        run: &str,
        private: bool,
    ) -> Result<Self> {
        let route = Self {
            communication: communication.into(),
            destination: destination.into(),
            scope: scope.into(),
            run: run.into(),
            private,
        };
        route.validate()?;
        Ok(route)
    }

    /// Validate the identifiers and endpoint supplied by trusted configuration.
    pub fn validate(&self) -> Result<()> {
        for id in [&self.communication, &self.scope, &self.run] {
            ensure!(valid_id(id), "invalid route identifier");
        }
        ensure!(
            !self.destination.is_empty()
                && !self.destination.starts_with('-')
                && !self.destination.chars().any(char::is_control),
            "invalid communication destination"
        );
        Ok(())
    }

    pub fn communication(&self) -> &str {
        &self.communication
    }

    pub fn destination(&self) -> &str {
        &self.destination
    }

    pub fn branch(&self, stream: Stream) -> String {
        format!("refs/heads/fuzukue/v1/{}/runs/{}/{stream}", self.scope, self.run)
    }

    pub fn event_id(&self, stream: Stream, seq: u64) -> String {
        format!("{}:{stream}:{seq}", self.run)
    }

    pub(super) fn header(&self, stream: Stream) -> Header {
        Header {
            version: 1,
            scope: self.scope.clone(),
            run: self.run.clone(),
            stream,
        }
    }

    pub(super) fn sequence(&self, id: &str, expected: Stream) -> Result<u64> {
        let (_, seq) = id.rsplit_once(':').context("invalid event reference")?;
        let seq: u64 = seq.parse().context("invalid event sequence")?;
        ensure!(seq > 0 && self.event_id(expected, seq) == id, "invalid event reference");
        Ok(seq)
    }
}

pub(super) fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    id.len() <= 64
        && bytes.next().is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(super) fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && oid.bytes().any(|byte| byte != b'0')
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Header {
    pub version: u32,
    pub scope: String,
    pub run: String,
    pub stream: Stream,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub repository: String,
    pub oid: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub verified: Vec<String>,
    #[serde(default)]
    pub unverified: Vec<String>,
    #[serde(default)]
    pub remaining: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub state: String,
    pub progress: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub question: String,
    pub options: Vec<DecisionOption>,
    pub recommended: Option<String>,
    pub blocking: bool,
    pub context_event: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionReply {
    pub request_id: String,
    pub option_id: String,
    pub note: Option<String>,
    pub corrects: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub task_id: String,
    pub repositories: Vec<String>,
    pub scope: String,
    pub acceptance: Vec<String>,
    pub heartbeat_seconds: u64,
    pub verification_policy: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    pub assignment_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub assignment_id: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub report_id: String,
    pub result: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub through: u64,
    pub event_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Payload {
    Report(Report),
    Heartbeat(Heartbeat),
    DecisionRequest(DecisionRequest),
    DecisionReply(DecisionReply),
    Assignment(Assignment),
    Instruction(Instruction),
    Stop(Stop),
    Complete(Complete),
    Ack(Ack),
}

impl Payload {
    pub fn is_ack(&self) -> bool {
        matches!(self, Self::Ack(_))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Event {
    pub version: u32,
    pub scope: String,
    pub run: String,
    pub stream: Stream,
    pub seq: u64,
    pub id: String,
    pub at: String,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    scope: String,
    run: String,
    stream: Stream,
    seq: u64,
    id: String,
    at: String,
    #[serde(rename = "type")]
    kind: String,
    payload: Box<RawValue>,
}

impl Event {
    pub(super) fn new(route: &Route, stream: Stream, seq: u64, at: DateTime<Utc>, payload: Payload) -> Self {
        let header = route.header(stream);
        Self {
            version: header.version,
            scope: header.scope,
            run: header.run,
            stream,
            seq,
            id: route.event_id(stream, seq),
            at: at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            payload,
        }
    }

    pub(super) fn parse(raw: &str, route: &Route, stream: Stream, seq: u64) -> Result<Self> {
        ensure!(raw.len() <= MAX_EVENT_BYTES, "event exceeds the size limit");
        let envelope: Envelope = serde_json::from_str(raw).context("invalid event envelope")?;
        let header = route.header(stream);
        ensure!(envelope.version == header.version, "unsupported protocol version");
        ensure!(
            envelope.scope == header.scope && envelope.run == header.run && envelope.stream == stream,
            "event does not match its route"
        );
        ensure!(seq != 0 && envelope.seq == seq, "event sequence does not match its path");
        ensure!(envelope.id == route.event_id(stream, seq), "invalid event ID");
        let at = DateTime::parse_from_rfc3339(&envelope.at).context("invalid event time")?;
        ensure!(
            at.offset().local_minus_utc() == 0
                && (envelope.at.ends_with('Z') || envelope.at.ends_with("+00:00")),
            "event time must be UTC"
        );

        let raw = envelope.payload.get();
        let payload = match (stream, envelope.kind.as_str()) {
            (Stream::Worker, "report") => Payload::Report(serde_json::from_str(raw)?),
            (Stream::Worker, "heartbeat") => Payload::Heartbeat(serde_json::from_str(raw)?),
            (Stream::Worker, "decision_request") if route.private => {
                Payload::DecisionRequest(serde_json::from_str(raw)?)
            }
            (Stream::Desk, "decision_reply") if route.private => {
                Payload::DecisionReply(serde_json::from_str(raw)?)
            }
            (Stream::Desk, "assignment") => Payload::Assignment(serde_json::from_str(raw)?),
            (Stream::Desk, "instruction") => Payload::Instruction(serde_json::from_str(raw)?),
            (Stream::Desk, "stop") => Payload::Stop(serde_json::from_str(raw)?),
            (Stream::Worker, "complete") => Payload::Complete(serde_json::from_str(raw)?),
            (_, "ack") => Payload::Ack(serde_json::from_str(raw)?),
            _ => bail!("event type is unavailable on this stream or route"),
        };
        validate_payload(&payload)?;
        Ok(Self {
            version: envelope.version,
            scope: envelope.scope,
            run: envelope.run,
            stream,
            seq,
            id: envelope.id,
            at: envelope.at,
            payload,
        })
    }
}

fn check_text(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty()
            && text.chars().all(|ch| !ch.is_control() || matches!(ch, '\t' | '\n' | '\r')),
        "empty text or unsupported character"
    );
    Ok(())
}

fn validate_payload(payload: &Payload) -> Result<()> {
    match payload {
        Payload::Report(report) => {
            check_text(&report.summary)?;
            for value in report.verified.iter().chain(&report.unverified).chain(&report.remaining) {
                check_text(value)?;
            }
            for artifact in &report.artifacts {
                ensure!(valid_id(&artifact.repository) && valid_oid(&artifact.oid), "invalid artifact");
                ensure!(
                    artifact.git_ref.starts_with("refs/")
                        && !artifact.git_ref.ends_with('/')
                        && !artifact.git_ref.contains("..")
                        && !artifact.git_ref.contains("@{")
                        && artifact.git_ref.split('/').all(|part| {
                            !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock")
                                && !part.ends_with('.')
                                && part.bytes().all(|ch| ch.is_ascii_graphic() && !b"~^:?*[\\".contains(&ch))
                        }),
                    "invalid artifact ref"
                );
            }
        }
        Payload::Heartbeat(heartbeat) => {
            check_text(&heartbeat.state)?;
            if let Some(progress) = &heartbeat.progress {
                check_text(progress)?;
            }
        }
        Payload::DecisionRequest(request) => {
            check_text(&request.question)?;
            ensure!(!request.options.is_empty(), "decision requires options");
            let mut ids = std::collections::HashSet::new();
            for option in &request.options {
                ensure!(valid_id(&option.id) && ids.insert(&option.id), "invalid or duplicate decision option");
                check_text(&option.label)?;
            }
            ensure!(
                request.recommended.as_ref().is_none_or(|id| ids.contains(id)),
                "unknown recommended option"
            );
        }
        Payload::DecisionReply(reply) => {
            ensure!(valid_id(&reply.option_id), "invalid selected option");
            if let Some(note) = &reply.note {
                check_text(note)?;
            }
        }
        Payload::Assignment(assignment) => {
            ensure!(
                valid_id(&assignment.task_id)
                    && assignment.repositories.iter().all(|id| valid_id(id))
                    && assignment.heartbeat_seconds > 0,
                "invalid assignment"
            );
            check_text(&assignment.scope)?;
            check_text(&assignment.verification_policy)?;
            for condition in &assignment.acceptance {
                check_text(condition)?;
            }
        }
        Payload::Instruction(instruction) => check_text(&instruction.text)?,
        Payload::Stop(stop) => {
            if let Some(reason) = &stop.reason {
                check_text(reason)?;
            }
        }
        Payload::Complete(complete) => check_text(&complete.result)?,
        Payload::Ack(ack) => ensure!(ack.through > 0, "invalid acknowledgement position"),
    }
    Ok(())
}
