//! Git-backed desk and worker exchange. The caller provisions the trusted route,
//! protects captured OIDs, and drives this synchronous API on a blocking worker.
//! Delivery and publication are explicit and survive reopening the local state.

mod delivery;
mod git;
mod store;
mod wire;

pub use delivery::{deliver_unread, Notification};
pub use git::{GitRepo, Oid, PublishPlan};
pub use store::{Capture, Observation, Quarantine, Receipt, Session};
pub use wire::{
    Ack, Artifact, Assignment, Complete, DecisionOption, DecisionReply, DecisionRequest, Event,
    Heartbeat, Instruction, Payload, Report, Route, Stop, Stream,
};

#[cfg(test)]
mod tests;
