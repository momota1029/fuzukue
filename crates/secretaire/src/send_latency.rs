use std::cell::{Cell, RefCell};
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fuzukue_proto::{Actor, ClientMsg, ServerEvent};

pub(crate) struct Received {
    at: Instant,
    threshold: Duration,
}

tokio::task_local! {
    static PROMPT: Prompt;
}

struct Prompt {
    received: Received,
    echo: RefCell<Option<(String, &'static str)>>,
    lock_wait: Cell<Duration>,
}

impl Received {
    pub(crate) fn now() -> Option<Self> {
        static THRESHOLD: OnceLock<Option<Duration>> = OnceLock::new();
        let threshold = (*THRESHOLD.get_or_init(|| {
            std::env::var("FUZUKUE_SEND_LATENCY_MS").ok()?.parse().ok().map(Duration::from_millis)
        }))?;
        Some(Self { at: Instant::now(), threshold })
    }

    pub(crate) fn for_message(received: Option<Self>, msg: &ClientMsg) -> Option<Self> {
        received.filter(|_| matches!(msg, ClientMsg::Prompt { .. } | ClientMsg::PromptWithAttachments { .. }
            | ClientMsg::RolePrompt { .. } | ClientMsg::WorkerPrompt { .. }))
    }

    pub(crate) async fn scope<T>(received: Option<Self>, handle: impl Future<Output = T>) -> T {
        if let Some(received) = received {
            let prompt = Prompt { received, echo: RefCell::default(), lock_wait: Cell::default() };
            PROMPT.scope(prompt, handle).await
        } else {
            handle.await
        }
    }
}

fn user_item(event: &ServerEvent) -> Option<&str> {
    match event {
        ServerEvent::UserPrompt { item, .. } | ServerEvent::TextDelta { item, actor: Actor::User, .. } => Some(item),
        _ => None,
    }
}

pub(crate) fn expect_echo(event: &ServerEvent, route: &'static str) {
    let _ = PROMPT.try_with(|prompt| {
        *prompt.echo.borrow_mut() = user_item(event).map(|item| (item.to_owned(), route));
    });
}

pub(crate) fn before_prompt_lock() -> Option<Instant> {
    PROMPT.try_with(|_| Instant::now()).ok()
}

pub(crate) fn after_prompt_lock(started: Option<Instant>) {
    if let Some(started) = started {
        PROMPT.with(|prompt| prompt.lock_wait.set(started.elapsed()));
    }
}

pub(crate) struct Publish {
    received_at: Instant,
    threshold: Duration,
    started_at: Instant,
    lock_wait: Duration,
    route: &'static str,
    pub(crate) stages: Option<Stages>,
}

pub(crate) struct Stages {
    pub(crate) locked_at: Instant,
    pub(crate) journal_started_at: Instant,
    pub(crate) journal_done_at: Instant,
    pub(crate) transcript_done_at: Instant,
    pub(crate) broadcast_at: Instant,
    pub(crate) seq: u64,
    pub(crate) transcript_error: bool,
}

impl Publish {
    pub(crate) fn start(event: &ServerEvent) -> Option<Self> {
        let item = user_item(event)?;
        PROMPT.try_with(|prompt| {
            let (_, route) = prompt.echo.borrow_mut().take_if(|(expected, _)| expected == item)?;
            Some(Self {
                received_at: prompt.received.at,
                threshold: prompt.received.threshold,
                started_at: Instant::now(),
                lock_wait: prompt.lock_wait.get(),
                route,
                stages: None,
            })
        }).ok().flatten()
    }

    pub(crate) fn log(self) {
        let Some(stages) = self.stages else { return };
        let elapsed = |at: Instant| at.duration_since(self.received_at).as_micros() as u64;
        if stages.broadcast_at.duration_since(self.received_at) >= self.threshold {
            tracing::warn!(target: "send_latency",
                seq = stages.seq,
                route = self.route,
                prompt_lock_wait_us = self.lock_wait.as_micros() as u64,
                hub_start_us = elapsed(self.started_at),
                hub_locked_us = elapsed(stages.locked_at),
                journal_start_us = elapsed(stages.journal_started_at),
                journal_done_us = elapsed(stages.journal_done_at),
                transcript_done_us = elapsed(stages.transcript_done_at),
                broadcast_us = elapsed(stages.broadcast_at),
                transcript_error = stages.transcript_error,
                "prompt receive to broadcast");
        }
    }
}
