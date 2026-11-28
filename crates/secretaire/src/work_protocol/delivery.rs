//! Transfer protocol receipts to a durable downstream notification queue.

use std::future::Future;

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::Session;

/// An owned notification that can be retried with the same identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    pub thread: String,
    pub text: String,
}

/// Deliver unread events in order. The callback must return success only after
/// it durably owns the notification, including when the ID was already accepted.
/// A failure leaves the current receipt unread for the next poll or restart.
pub async fn deliver_unread<F, Fut>(
    session: &mut Session,
    thread: &str,
    mut accept: F,
) -> Result<usize>
where
    F: FnMut(Notification) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut delivered = 0;
    loop {
        let receipt = session.unread().next().cloned();
        let Some(receipt) = receipt else { break };
        // Include the complete scope as event IDs alone only identify a run.
        let identity = serde_json::to_vec(&(session.route().communication(), &receipt.event.scope, &receipt.event.id))?;
        let id = format!("work-v1:{:x}", Sha256::digest(identity));
        let note = Notification {
            id,
            thread: thread.to_owned(),
            text: serde_json::to_string_pretty(&receipt.event)?,
        };
        accept(note).await?;
        session.mark_delivered(&receipt.event.id)?;
        delivered += 1;
    }
    Ok(delivered)
}
