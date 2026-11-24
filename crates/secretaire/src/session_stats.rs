//! タイムラインと独立した、会話・エージェントごとの統計。

use std::collections::BTreeMap;
use std::sync::Mutex;

use fuzukue_proto::{AgentBackend, AgentSelection, Envelope, RunDetails, ServerEvent};
use tokio::sync::broadcast;

use crate::hub::Hub;

#[derive(Default)]
pub(crate) struct SessionStats {
    pub model: String,
    pub effort: Option<String>,
    pub cost_usd: Option<f64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: Option<u64>,
    pub turns: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub context_breakdown: Option<BTreeMap<String, u64>>,
    pub auto_compact_threshold: Option<u64>,
    pub mcp_tools: Option<Vec<String>>,
    pub connected: Option<bool>,
    pub(crate) backend_id: Option<String>,
    pub(crate) context_notice_sent: bool,
    pub(crate) reported_tokens: u64,
    pub(crate) reported_cost: f64,
}

impl SessionStats {
    fn event(&self, session: &str, agent: &str) -> ServerEvent {
        ServerEvent::SessionStats {
            session: session.into(),
            agent: agent.into(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            cost_usd: self.cost_usd,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            thinking_tokens: self.thinking_tokens,
            turns: self.turns,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            context_breakdown: self.context_breakdown.clone(),
            auto_compact_threshold: self.auto_compact_threshold,
            mcp_tools: self.mcp_tools.clone(),
            connected: self.connected,
        }
    }
}

#[derive(Default)]
pub(crate) struct SessionStatsStore {
    current: Mutex<BTreeMap<(String, &'static str), SessionStats>>,
}

impl SessionStatsStore {
    pub fn update(&self, hub: &Hub, session: &str, agent: &'static str, update: impl FnOnce(&mut SessionStats)) {
        let mut current = self.current.lock().unwrap();
        let stats = current.entry((session.into(), agent)).or_default();
        update(stats);
        hub.publish_snapshot(stats.event(session, agent));
        let percent = crate::commands::CONTEXT_NOTICE_PERCENT;
        if stats.context_notice_sent || !stats.context_tokens.zip(stats.context_window).is_some_and(|(used, window)| {
            window > 0 && u128::from(used) * 100 > u128::from(window) * u128::from(percent)
        }) {
            return;
        }
        if let Some(id) = stats.backend_id.as_ref().filter(|_| hub.has_thread(session)) {
            stats.context_notice_sent = true;
            let item = format!("context-notice:{agent}:{id}");
            if !hub.read_threads(|store| store.has_item(session, &item)) {
                crate::commands::notice_with_id(hub, session, item, fuzukue_proto::LocalizedMessage::new(
                    "command-context-high", [("percent", percent.to_string())],
                ));
            }
        }
    }

    pub(crate) fn identify(&self, session: &str, backend: AgentBackend, id: &str) {
        let mut current = self.current.lock().unwrap();
        let stats = current.entry((session.into(), backend.usage_key())).or_default();
        if stats.backend_id.as_deref() != Some(id) {
            stats.backend_id = Some(id.into());
            stats.context_notice_sent = false;
            stats.context_tokens = None;
            stats.context_window = None;
            stats.context_breakdown = None;
        }
    }

    pub(crate) fn context_tokens(&self, session: &str, backend: AgentBackend) -> Option<u64> {
        self.current.lock().unwrap().get(&(session.into(), backend.usage_key()))?.context_tokens
    }

    pub(crate) fn reset_context(&self, hub: &Hub, session: &str) {
        let mut current = self.current.lock().unwrap();
        for backend in AgentBackend::ALL {
            if let Some(stats) = current.get_mut(&(session.into(), backend.usage_key())) {
                stats.backend_id = None;
                stats.context_notice_sent = false;
                stats.context_tokens = None;
                stats.context_window = None;
                stats.context_breakdown = None;
                stats.connected = Some(false);
                hub.publish_snapshot(stats.event(session, backend.usage_key()));
            }
        }
    }

    pub fn take_details(&self, session: &str, backend: AgentBackend, duration_ms: Option<u64>) -> Option<RunDetails> {
        let mut current = self.current.lock().unwrap();
        let stats = current.get_mut(&(session.to_owned(), backend.usage_key()))?;
        let total = stats.input_tokens.saturating_add(stats.output_tokens);
        let tokens = (total > 0).then(|| total.saturating_sub(stats.reported_tokens.min(total)));
        stats.reported_tokens = total;
        let cost_usd = stats.cost_usd.map(|cost| (cost - stats.reported_cost).max(0.0));
        stats.reported_cost = stats.cost_usd.unwrap_or_default();
        Some(RunDetails {
            selection: AgentSelection {
                backend, model: (!stats.model.is_empty()).then(|| stats.model.clone()), effort: stats.effort.clone(),
            },
            escalated: false, tokens, cost_usd, duration_ms,
        })
    }

    pub fn subscribe(
        &self,
        hub: &Hub,
        after: u64,
        include_stats: bool,
        include_threads: bool,
        transcripts: Option<&crate::hub::TranscriptResume>,
    ) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        // 購読開始と統計の読み取りを直列化し、古いライブ更新が初期値の後に届くのを防ぐ。
        let current = self.current.lock().unwrap();
        let (mut initial, live) = match transcripts {
            Some(resume) => hub.subscribe_transcripts(after, resume),
            None => hub.subscribe_client(after, include_threads),
        };
        if include_stats {
            let index = initial.len() - usize::from(transcripts.is_some());
            initial.splice(index..index, current.iter().map(|((session, agent), stats)| Envelope {
                workspace: None,
                seq: 0,
                event: stats.event(session, agent),
            }));
        }
        (initial, live)
    }
}
