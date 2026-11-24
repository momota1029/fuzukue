use std::sync::Weak;

use fuzukue_proto::{Envelope, GitHubActivity, GitHubLink, ServerEvent};
use tokio::sync::broadcast;

use super::{url_link, GitHubConfig, Kind, Workspace};

mod command;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Expected {
    Opened,
    Comment {
        body: Option<String>,
        id: Option<u64>,
    },
    Activity(Kind),
    Label(Kind, String),
    RemovedLabels(Vec<String>),
}

impl Expected {
    pub(super) fn comment(body: Option<String>) -> Self {
        Self::Comment { body, id: None }
    }

    fn matches(&self, event: &GitHubActivity) -> bool {
        let opened = event.id.ends_with(":opened");
        match self {
            Self::Opened => opened,
            Self::Comment { body, id } => {
                !opened
                    && event.kind == Kind::Comment
                    && if let Some(id) = id {
                        event
                            .id
                            .rsplit(':')
                            .nth(1)
                            .and_then(|part| part.parse().ok())
                            == Some(*id)
                    } else {
                        body.as_ref().is_none_or(|body| {
                            event.body.as_deref().map(str::trim) == Some(body.trim())
                        })
                    }
            }
            Self::Activity(kind) => !opened && event.kind == *kind,
            Self::Label(kind, name) => {
                event.kind == *kind && event.excerpt.eq_ignore_ascii_case(name)
            }
            Self::RemovedLabels(kept) => {
                event.kind == Kind::Unlabeled
                    && !kept
                        .iter()
                        .any(|label| label.eq_ignore_ascii_case(&event.excerpt))
            }
        }
    }
}

struct Write {
    session: String,
    link: GitHubLink,
    at: i64,
    tool: Option<String>,
    expected: Vec<(Expected, Vec<String>)>,
}

#[derive(Default)]
pub(super) struct Ledger {
    writes: Vec<Write>,
}

fn fresh(now: i64, at: i64, window: u64) -> bool {
    now.abs_diff(at) < window && now >= at
}

impl Ledger {
    pub(super) fn sources(&self, link: &GitHubLink) -> std::collections::BTreeSet<String> {
        self.writes.iter().filter(|write| &write.link == link)
            .map(|write| write.session.clone()).collect()
    }

    fn expire(&mut self, now: i64, window: u64) {
        self.writes
            .retain(|write| now < write.at || fresh(now, write.at, window));
    }

    pub(super) fn record(
        &mut self,
        session: &str,
        link: &GitHubLink,
        expected: Vec<Expected>,
        at: i64,
        window: u64,
        tool: Option<&str>,
    ) {
        self.expire(at, window);
        if !expected.is_empty()
            && window > 0
            && tool.is_none_or(|tool| {
                !self
                    .writes
                    .iter()
                    .any(|write| write.session == session && write.tool.as_deref() == Some(tool))
            })
        {
            self.writes.push(Write {
                session: session.into(),
                link: link.clone(),
                at,
                tool: tool.map(str::to_owned),
                expected: expected
                    .into_iter()
                    .map(|event| (event, Vec::new()))
                    .collect(),
            });
        }
    }

    pub(super) fn since(&mut self, link: &GitHubLink, now: i64, window: u64) -> Option<i64> {
        self.expire(now, window);
        self.writes
            .iter()
            .filter(|write| &write.link == link)
            .map(|write| write.at)
            .min()
    }

    pub(super) fn matches(
        &mut self,
        session: &str,
        event: &GitHubActivity,
        user: &str,
        now: i64,
        window: u64,
    ) -> bool {
        self.expire(now, window);
        let owner = event
            .link
            .repo
            .split_once('/')
            .map_or("", |(owner, _)| owner);
        if !super::owner_account(&event.actor, owner, user) {
            return false;
        }
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&event.at) else {
            return false;
        };
        let relevant = |write: &Write| {
            write.session == session
                && write.link == event.link
                && fresh(at.timestamp(), write.at, window)
        };
        if self
            .writes
            .iter()
            .filter(|write| relevant(write))
            .flat_map(|write| &write.expected)
            .any(|(_, ids)| ids.contains(&event.id))
        {
            return true;
        }
        let mut candidates = self
            .writes
            .iter_mut()
            .filter(|write| relevant(write))
            .flat_map(|write| &mut write.expected);
        if let Some((_, ids)) = candidates.find(|(expected, ids)| {
            (ids.is_empty() || matches!(expected, Expected::RemovedLabels(_)))
                && expected.matches(event)
        }) {
            ids.push(event.id.clone());
            return true;
        }
        false
    }

    fn complete(&mut self, session: &str, item: &str, ok: bool, output: &str) {
        for write in self
            .writes
            .iter_mut()
            .filter(|write| write.session == session && write.tool.as_deref() == Some(item))
        {
            // Close and edit can apply one change before failing on another.
            if !ok
                && matches!(&write.expected[..], [(Expected::Comment { .. }, ids)] if ids.is_empty())
            {
                write.expected.clear();
            }
            if ok {
                if let Some(id) = comment_id(output, &write.link) {
                    for (expected, _) in &mut write.expected {
                        if let Expected::Comment {
                            id: expected_id, ..
                        } = expected
                        {
                            *expected_id = Some(id);
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn comment_id(output: &str, link: &GitHubLink) -> Option<u64> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
        return value["id"].as_u64();
    }
    let url = output.trim().lines().last()?.trim();
    if url_link(url).ok().as_ref() != Some(link) {
        return None;
    }
    url.split_once("#issuecomment-")?.1.parse().ok()
}

#[derive(Default)]
pub(super) struct Observer {
    creates: Vec<(String, String, String, i64, Vec<Expected>)>,
}

impl Observer {
    pub(super) async fn run(
        mut self,
        app: Weak<Workspace>,
        mut events: broadcast::Receiver<Envelope>,
    ) {
        loop {
            match events.recv().await {
                Ok(envelope) => {
                    let Some(app) = app.upgrade() else { return };
                    self.observe(&app, &envelope.event, chrono::Utc::now().timestamp());
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!("GitHub write observer missed {count} workspace events");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    pub(super) fn observe(&mut self, app: &Workspace, event: &ServerEvent, now: i64) {
        match event {
            ServerEvent::ToolCall {
                session,
                item,
                name,
                input,
                ..
            } if matches!(name.as_str(), "Bash" | "exec") => {
                let Some(command) = input["command"].as_str() else {
                    return;
                };
                let Ok(repo) = app.research_issues.repo() else {
                    return;
                };
                let Some(write) = command::parse(command, repo) else {
                    return;
                };
                let Some(state) = app.hub.read_threads(|store| store.github_state(session)) else {
                    return;
                };
                let Ok(config) = GitHubConfig::from_workspace(&app.project_dir) else {
                    return;
                };
                let window = config.echo_window_secs;
                self.creates
                    .retain(|(_, _, _, at, _)| fresh(now, *at, window));
                match write {
                    command::Write::Item(link, expected) if state.links.contains(&link) => {
                        app.issue_threads.echo.lock().unwrap().record(
                            session,
                            &link,
                            expected,
                            now,
                            window,
                            Some(item),
                        );
                    }
                    command::Write::Create(repo, expected)
                        if window > 0 && state.links.iter().any(|link| link.repo == repo) =>
                    {
                        if !self
                            .creates
                            .iter()
                            .any(|(id, tool, ..)| id == session && tool == item)
                        {
                            self.creates
                                .push((session.clone(), item.clone(), repo, now, expected));
                        }
                    }
                    _ => {}
                }
            }
            ServerEvent::ToolResult {
                session,
                item,
                ok,
                output,
            } => {
                app.issue_threads
                    .echo
                    .lock()
                    .unwrap()
                    .complete(session, item, *ok, output);
                let Some(index) = self
                    .creates
                    .iter()
                    .position(|(id, tool, ..)| id == session && tool == item)
                else {
                    return;
                };
                let (_, _, repo, at, expected) = self.creates.swap_remove(index);
                let Some(link) = ok
                    .then(|| {
                        output
                            .trim()
                            .lines()
                            .last()
                            .and_then(|url| url_link(url.trim()).ok())
                    })
                    .flatten()
                else {
                    return;
                };
                if link.repo != repo {
                    return;
                }
                // A creation receives its number from GitHub. Delivery still
                // requires this exact item to be linked to the originating thread.
                if let Ok(config) = GitHubConfig::from_workspace(&app.project_dir) {
                    if fresh(now, at, config.echo_window_secs) {
                        app.issue_threads.echo.lock().unwrap().record(
                            session,
                            &link,
                            expected,
                            at,
                            config.echo_window_secs,
                            Some(item),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}
