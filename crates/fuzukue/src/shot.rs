//! Scenario playback for the real Workbench on an isolated Xvfb display.

use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use gpui_kit::{
    point, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, Context, Entity, Keystroke,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, WindowBounds, WindowOptions,
};
use fuzukue_view::appearance::{AppearanceSettings, ThemePreference};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde::Deserialize;

use super::{
    conn, math, px, ClientMsg, ReadRow, RightPane, ServerEvent, Window, Workbench,
};

#[cfg(test)]
mod deck_overview_test;
#[cfg(test)]
mod agent_models_test;
#[cfg(test)]
mod approval_dock_test;
#[cfg(test)]
mod workspace_requests_test;
#[cfg(test)]
mod file_browser_test;
#[cfg(test)]
mod github_report_test;
#[cfg(test)]
mod pane_resize_test;
#[cfg(test)]
mod question_card_test;
mod scroll_regression;
mod selection_region;
mod thread_navigation;

const TIMEOUT_MS: u64 = 15_000;

#[derive(Default)]
pub(super) struct Activity {
    counts: HashMap<String, usize>,
    last: Option<Instant>,
    pub(super) card_detail_scroll: std::cell::RefCell<Option<gpui_kit::ScrollHandle>>,
}

impl Activity {
    pub(super) fn record(&mut self, event: &ServerEvent) {
        if let Ok(value) = serde_json::to_value(event) {
            if let Some(kind) = value["type"].as_str() {
                *self.counts.entry(kind.into()).or_default() += 1;
            }
        }
        self.last = Some(Instant::now());
    }

    fn count(&self, kind: &str) -> usize {
        self.counts.get(kind).copied().unwrap_or(0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    #[serde(default, rename = "auto_review")]
    _auto_review: bool,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Size {
        width: u32,
        height: u32,
    },
    Locale { locale: String },
    Theme {
        theme: ShotTheme,
    },
    CommentDestination {
        side_thread: bool,
    },
    Connect,
    ModelSetupFixture,
    DistributionPreview,
    UsagePreview,
    ClientDownloadPreview { resuming: bool },
    ClientRollbackPreview,
    GitHubReportPreview,
    ScreenshotCaptureFixture,
    DropFolderFixture,
    SelfUpdatePreview {
        #[serde(default)] canary: bool,
        #[serde(default)] waiting: bool,
    },
    WaitEvent {
        event: String,
        timeout_ms: Option<u64>,
    },
    ResolveApproval { decision: String },
    SeedApprovalDock,
    SeedIntegrateApproval,
    SeedCommandApproval,
    SeedComposerContext,
    SeedGoalStatus { reason: String },
    AssertApprovalDockScrolled,
    ResolveApprovalDock,
    WaitResearchIssue { number: u64, state: fuzukue_proto::ResearchIssueState },
    WaitGit {
        path: Option<String>,
        diff: Option<String>,
        commit: Option<String>,
    },
    WaitIdle {
        quiet_ms: Option<u64>,
        timeout_ms: Option<u64>,
    },
    OpenWorkspace { path: String, path_tail: String },
    SwitchWorkspace { path_tail: String },
    SelectThread {
        title: String,
    },
    SeedThreadSwitch,
    SeedChatReply { text: String },
    AssertThreadSwitch {
        session: String,
        restore: bool,
    },
    AssertActiveThread {
        title: String,
    },
    OpenThreadMenu,
    HoverThread { title: String },
    WaitThreadPinned { title: String },
    WaitThreadArchived {
        title: String,
    },
    AssertModel { model: String },
    AssertWorkerModel { model: String },
    AssertDelegateReport { model: String },
    OpenDelegateDetails,
    WaitDelegateText { text: String },
    WaitDelegateSummaries { count: usize },
    WaitPartnerText { text: String },
    WaitResponding { status: String },
    Upload { name: String, text: String },
    ClipboardImage,
    WaitElement { id: String },
    WaitNote { path: String, text: String },
    HoverSource { line: u32, col: u32 },
    WaitSourceDiagnostics,
    WaitSourceHover,
    WaitTrashTitle { title: String },
    AssertOrder { above: String, below: String },
    AssertThreadInfoLayout { open: bool },
    WaitInboxProposal { to: String },
    WaitReviews { deck: String, kinds: Vec<String>, running: bool },
    StartComment {
        card: String,
        quote: Option<String>,
    },
    NewThread {
        #[serde(default)]
        deck: Option<String>,
    },
    AttachDeck {
        deck: String,
    },
    OpenPaper {
        deck: String,
        via: Option<String>,
    },
    Scroll {
        pane: Pane,
        pixels: f32,
    },
    /// 本物のホイールと同じく、ScrollEvent をペインの中央に送る。
    Wheel {
        pane: Pane,
        #[serde(default)]
        pixels: Option<f32>,
        #[serde(default)]
        lines: Option<f32>,
        #[serde(default)]
        pages: Option<f32>,
    },
    /// ペインのスクロール位置を標準エラーに出す。
    LogScroll,
    ScrollToCard {
        id: String,
    },
    WaitPaperCard {
        deck: String,
        card: String,
        text: Option<String>,
    },
    ExpandProofs,
    BuildPaper,
    WaitPdf,
    ClickPdfCard {
        card: String,
        #[serde(default)]
        ctrl: bool,
    },
    Click {
        id: Option<String>,
        x: Option<f32>,
        y: Option<f32>,
    },
    CtrlClickText { card: String, text: String },
    Hover { id: String },
    Drag {
        from_x: f32,
        from_y: f32,
        to_x: f32,
        to_y: f32,
    },
    DragFromElement {
        id: String,
        dx: f32,
        dy: f32,
    },
    AssertCardScrollbarDrag,
    AssertPaneTextSelection,
    AssertTranscriptScrollGrowth,
    AssertThreadNavigationKeys,
    AssertChatReplayLoad,
    AssertThreadHistoryPaging,
    AssertSidebarWidth {
        width: f32,
    },
    AssertFileTreeLabels { paths: Vec<String> },
    Type {
        text: String,
    },
    AssertComposerText {
        text: String,
    },
    AssertComposerDecorations {
        styled: usize,
        dimmed: usize,
    },
    Key {
        key: String,
    },
    Wait {
        ms: u64,
    },
    Screenshot {
        name: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ShotTheme {
    Light,
    Dark,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Pane {
    Paper,
    Chat,
    ApprovalDock,
}

enum ClickTarget {
    Element { id: String },
    Coordinates { x: f32, y: f32 },
}

pub(super) fn main() -> Result<()> {
    ensure!(
        fuzukue_proto::compat::env("FUZUKUE_SHOT_XVFB").as_deref() == Ok("1"),
        "launch this mode with scripts/shot.sh"
    );
    let args: Vec<_> = std::env::args_os().skip(2).collect();
    let [scenario, output, url, mode] = args.as_slice() else {
        bail!("expected scenario, output directory, WebSocket URL and connection mode");
    };
    let scenario: Scenario = serde_json::from_slice(&std::fs::read(scenario)?)
        .with_context(|| format!("reading {}", Path::new(scenario).display()))?;
    let output = PathBuf::from(output);
    let read_only = mode == "read-only";
    ensure!(read_only || mode == "fixture", "invalid connection mode");
    let url = url.to_str().context("WebSocket URL must be UTF-8")?;
    let target = conn::Target::for_shot(url, read_only)?;

    let result = std::rc::Rc::new(std::cell::RefCell::new(None));
    let completed = result.clone();
    gpui_kit::platform::linux(gpui_kit::WindowingModes::X11)
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
            math::init(cx);
            let (handle, view) = match open_workbench(cx) {
                Ok(window) => window,
                Err(err) => {
                    *completed.borrow_mut() = Some(Err(err));
                    cx.quit();
                    return;
                }
            };
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle,
                    view,
                    target: Some(target),
                    output,
                    read_only,
                    seen: HashMap::new(),
                };
                *completed.borrow_mut() = Some(runner.run(scenario, cx).await);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    let outcome = result
        .borrow_mut()
        .take()
        .context("capture window closed before the scenario finished")?;
    outcome
}

fn open_workbench(cx: &mut App) -> Result<(AnyWindowHandle, Entity<Workbench>)> {
    open_workbench_at_size(cx, 1440., 900.)
}

fn open_workbench_at_size(cx: &mut App, width: f32, height: f32) -> Result<(AnyWindowHandle, Entity<Workbench>)> {
    crate::appearance::register_window_typography(cx);
    let bounds = Bounds::new(Default::default(), size(px(width), px(height)));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..gpui_kit::component::TitleBar::window_options()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("fuzukue-shot");
        cx.new(|cx| {
            let mut workbench = Workbench::new_with_target(window, cx, Some(Err("Not connected")));
            workbench.shot_activity = Some(Activity::default());
            workbench
        })
    })
}

struct Runner {
    handle: AnyWindowHandle,
    view: Entity<Workbench>,
    target: Option<conn::Target>,
    output: PathBuf,
    read_only: bool,
    seen: HashMap<String, usize>,
}

const THREAD_SWITCH_ROWS: usize = 300;

fn seed_composer_context(view: &mut Workbench, cx: &mut Context<Workbench>) {
    let session = String::from("composer-context-fixture");
    view.protocol = fuzukue_proto::PROTOCOL_VERSION;
    view.threads_loaded = true;
    view.switch_session(session.clone(), cx);
    view.chats.get_mut(&session).unwrap().settings.permission_mode =
        fuzukue_proto::PermissionMode::BypassPermissions;
    view.on_event(ServerEvent::ChatCapabilities {
        effort_levels: vec![], mail_restricted: false, mcp_tools: vec![], folder_attachments: false,
    }, cx);
    view.on_event(ServerEvent::WorkerApprovalPolicyChanged {
        policy: fuzukue_proto::WorkerApprovalPolicy::Auto, shell_rules: None,
    }, cx);
    let model = "gpt-fixture-model-with-a-long-provider-identifier-and-release-suffix";
    let started_at_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as u64;
    view.on_event(ServerEvent::ToolCall {
        session: session.clone(), item: "delegate:composer-context".into(),
        actor: fuzukue_proto::Actor::System, name: "delegate".into(),
        input: serde_json::json!({
            "title": "原稿の確認", "model": model, "started_at_ms": started_at_ms,
            "selection": {"backend": "codex", "model": model, "effort": "high"},
            "routing": {"tier": "heavy", "reason": "Fixture"}
        }),
    }, cx);
    view.on_event(ServerEvent::SessionStats {
        session, agent: "codex".into(), model: model.into(), effort: Some("high".into()),
        context_tokens: Some(813_577), context_window: Some(1_000_000),
        input_tokens: 813_577, output_tokens: 120, thinking_tokens: None,
        cost_usd: None, cache_read_tokens: None, cache_write_tokens: None,
        context_breakdown: None, auto_compact_threshold: None, mcp_tools: None,
        connected: Some(true), turns: 2,
    }, cx);
}

fn seed_thread_switch(view: &mut Workbench, cx: &mut Context<Workbench>) {
    use fuzukue_proto::Actor;

    view.protocol = fuzukue_proto::PROTOCOL_VERSION;
    view.workspaces.active = Some("thread-switch-fixture".into());
    let threads = [("thread-switch-a", "History A"), ("thread-switch-b", "History B")];
    for (session, title) in threads {
        for index in 0..THREAD_SWITCH_ROWS {
            if index % 18 == 3 {
                let item = format!("tool:{index}");
                view.on_event(ServerEvent::ToolCall {
                    session: session.into(), item: item.clone(), actor: Actor::Claude,
                    name: "Read".into(), input: serde_json::json!({"file_path": format!("notes/{index}.md")}),
                }, cx);
                view.on_event(ServerEvent::ToolResult {
                    session: session.into(), item, ok: true, output: format!("Read note {index}"),
                }, cx);
                continue;
            }
            let (actor, item, text) = if index % 2 == 0 {
                (Actor::User, format!("user:{index}"), format!("{title} · question {}", index + 1))
            } else if index == 139 {
                (Actor::Claude, format!("reply:{index}"), format!("{{\"history\":\"{title}\",\"row\":{index}}}"))
            } else {
                let answer = "The detail wraps at narrow widths and includes `inline code`.";
                let mut text = format!("**{title} · reply {}**\n\n{answer}", index + 1);
                if index % 7 == 0 {
                    text.push_str("\n\n```text\n");
                    text.push_str(&"A line from the code sample.\n".repeat(1 + index % 5));
                    text.push_str("```");
                }
                (Actor::Claude, format!("reply:{index}"), text)
            };
            view.on_event(ServerEvent::TextDelta { session: session.into(), item, actor, text }, cx);
        }
        view.on_event(ServerEvent::TurnEnded { session: session.into(), actor: Actor::Claude }, cx);
    }
    let threads = threads.map(|(id, title)| {
        serde_json::from_value(serde_json::json!({"id": id, "title": title, "updated": u64::from(id == "thread-switch-a")})).unwrap()
    }).to_vec();
    view.on_event(ServerEvent::Threads { threads }, cx);
    view.select_thread("thread-switch-a".into(), cx);
}

impl Runner {
    async fn run(mut self, scenario: Scenario, cx: &mut AsyncApp) -> Result<()> {
        for (index, step) in scenario.steps.into_iter().enumerate() {
            self.step(step, cx)
                .await
                .with_context(|| format!("scenario step {}", index + 1))?;
        }
        Ok(())
    }

    async fn step(&mut self, step: Step, cx: &mut AsyncApp) -> Result<()> {
        self.frame(cx)?;
        match step {
            Step::Size { width, height } => {
                ensure!(
                    (320..=3840).contains(&width) && (240..=2160).contains(&height),
                    "size must be within 320×240 and 3840×2160"
                );
                let target = size(px(width as f32), px(height as f32));
                cx.update_window(self.handle, |_, window, _| window.resize(target))?;
                self.wait_for(cx, "window resize", TIMEOUT_MS, |_, window| {
                    window.viewport_size() == target
                })
                .await?;
            }
            Step::CommentDestination { side_thread } => {
                self.update(cx, |view, _, cx| {
                    view.appearance.remember_comment_destination(!side_thread)?;
                    cx.notify();
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::Locale { locale } => {
                ensure!(fuzukue_i18n::Locale::supported(&locale).is_some(), "unsupported shot locale {locale}");
                self.update(cx, |view, window, cx| view.shot_locale(locale, window, cx))?;
            }
            Step::Theme { theme } => {
                let theme = match theme {
                    ShotTheme::Light => ThemePreference::Light,
                    ShotTheme::Dark => ThemePreference::Dark,
                };
                self.update(cx, |view, window, cx| {
                    view.appearance.shot_theme(theme, window, cx);
                    cx.notify();
                })?;
            }
            Step::Connect => {
                let target = self
                    .target
                    .take()
                    .context("connect may only be used once")?;
                self.update(cx, |view, _, _| view.connection.restart(target))?;
                self.wait_for(
                    cx,
                    "initial workspace, document and threads",
                    TIMEOUT_MS,
                    |view, _| {
                        let activity = view.shot_activity.as_ref().unwrap();
                        view.protocol > 0
                            && view.threads_loaded
                            && activity.count("workspace") > 0
                            && activity.count("document") > 0
                    },
                )
                .await?;
            }
            Step::ModelSetupFixture => {
                ensure!(!self.read_only, "model setup requires an isolated fixture");
                let directory = tempfile::tempdir()?.keep();
                self.update(cx, |view, _, cx| {
                    view.agent_models = crate::agent_models::ModelSettings::load(Some(directory.join("settings.toml")));
                    cx.notify();
                })?;
            }
            Step::DistributionPreview => {
                self.update(cx, |view, _, cx| {
                    view.appearance.server_mode = fuzukue_view::server::ServerMode::Local;
                    view.appearance.host_tailnet = true;
                    view.appearance.keep_running = false;
                    view.distribution.public_url = Some("ws://192.0.2.11:7878/ws".into());
                    view.distribution.manifest = Some(fuzukue_proto::ClientManifest {
                        platform: "linux-x86_64".into(),
                        commit: "preview".into(),
                        built_at: "2026-11-20T00:00:00Z".into(),
                        sha256: String::new(),
                        size: 1,
                    });
                    cx.notify();
                })?;
            }
            Step::ClientDownloadPreview { resuming } => {
                self.update(cx, |view, _, cx| {
                    view.preview_client_download(resuming);
                    cx.notify();
                })?;
            }
            Step::ClientRollbackPreview => {
                self.update(cx, |view, _, cx| {
                    view.preview_client_rollback();
                    cx.notify();
                })?;
            }
            Step::GitHubReportPreview => {
                ensure!(!self.read_only, "GitHub report preview needs an isolated fixture");
                self.update(cx, |view, _, cx| seed_github_report(view, None, cx))?;
            }
            Step::ScreenshotCaptureFixture => {
                ensure!(!self.read_only, "Screenshot capture fixture needs an isolated workbench");
                let png = screenshot_fixture_png()?;
                self.update(cx, |view, _, _| view.stub_screenshot_capture(Ok(png)))?;
            }
            Step::DropFolderFixture => {
                ensure!(!self.read_only, "Folder drop needs an isolated fixture");
                let directory = tempfile::tempdir()?;
                // The shot runner removes its temporary directory when the scenario ends.
                let folder = directory.keep().join("sample-repository");
                std::fs::create_dir_all(folder.join(".git"))?;
                std::fs::write(folder.join("notes.txt"), "Synthetic attachment fixture\n")?;
                self.update(cx, |view, _, cx| {
                    view.add_attachments(view.active.clone(), vec![folder], cx);
                })?;
            }
            Step::UsagePreview => {
                ensure!(!self.read_only, "usage preview needs an isolated fixture");
                self.update(cx, |view, _, cx| {
                    let now = crate::unix_now();
                    let window = |agent: &str, name: &str, used, reset_in, rate: Option<f64>| fuzukue_proto::UsageWindow {
                        agent: agent.into(),
                        name: name.into(),
                        used,
                        resets_at: Some(now + reset_in),
                        rejected: false,
                        recent_pace: rate.map(|used_per_hour| fuzukue_proto::UsagePace {
                            observed_at: now,
                            used_per_hour,
                        }),
                    };
                    view.right = RightPane::Overview;
                    view.on_event(ServerEvent::Usage {
                        windows: vec![
                            window("claude", "5h", 0.89, 900, Some(0.33)),
                            window("claude", "7d", 0.80, 7200, Some(0.18)),
                            window("codex", "7d", 0.57, 36_000, None),
                        ],
                        pause_at: 0.95,
                    }, cx);
                    cx.notify();
                })?;
            }
            Step::SelfUpdatePreview { canary, waiting } => {
                self.update(cx, |view, _, cx| {
                    use fuzukue_proto::{SelfUpdateBlocker, SelfUpdateBlockerKind as BlockerKind, SelfUpdateStatus,
                        SelfUpdateStep, SelfUpdateStepKind as Kind, SelfUpdateStepState as State};
                    let workspace = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
                    let thread = "thread-0123456789abcdef0123456789abcdef";
                    let thread_title = "更新待機画面を実際の作業に近い条件で確認する";
                    view.update_self_update(SelfUpdateStatus {
                        build_id: "0123456789abcdef0123456789abcdef01234567".into(),
                        source_commit: Some("123456789abcdef0123456789abcdef012345678".into()),
                        available: true,
                        can_rollback: true,
                        active: true,
                        planner_work_held: waiting.then_some(2),
                        wait: waiting.then(|| fuzukue_proto::SelfUpdateWait {
                            started_at: 1,
                            elapsed_secs: 365,
                            reason: fuzukue_proto::SelfUpdateWaitReason {
                                threads: vec![fuzukue_proto::SelfUpdateBlockingThread {
                                    workspace: workspace.into(),
                                    id: thread.into(),
                                    title: Some(thread_title.into()),
                                    running: true,
                                    worker: true,
                                    queued: true,
                                    paused: false,
                                    checking: false,
                                }],
                                gate_held: true,
                                checking_sessions: false,
                                blockers: [
                                    (BlockerKind::Turn, Some(thread), thread, thread_title, 355_001),
                                    (BlockerKind::Worker, Some(thread), "delegate:ABcdefGhiJklmnOPqrsTuvwXyz123456789",
                                        "サーバーテストと更新待機の表示を確認する", 285_001),
                                    (BlockerKind::Approval, Some(thread), "git:preview", thread_title, 34_001),
                                    (BlockerKind::Test, None, "secretaire", "secretaire", 312_001),
                                ].into_iter().map(|(kind, session, id, title, started_at)| SelfUpdateBlocker {
                                    workspace: workspace.into(), session: session.map(str::to_owned),
                                    id: id.into(), title: Some(title.into()), kind, started_at,
                                }).collect(),
                            },
                            forcing: false,
                        }),
                        canary_status: canary.then(|| fuzukue_proto::SelfUpdateCanaryStatus {
                            state: fuzukue_proto::SelfUpdateCanaryState::Running,
                            commit: Some("123456789abcdef0123456789abcdef012345678".into()),
                            port: Some(7879),
                            connect_url: Some("ws://127.0.0.1:7879/ws".into()),
                            started_at: None,
                            auto_discard_at: None,
                            log: String::new(),
                        }),
                        steps: [Kind::Check, Kind::Build, Kind::Test, Kind::Shot, Kind::Publish, Kind::Wait, Kind::Swap]
                            .into_iter().map(|kind| SelfUpdateStep {
                                kind,
                                state: match kind {
                                    Kind::Check | Kind::Build => State::Passed,
                                    Kind::Test if canary || waiting => State::Passed,
                                    Kind::Test => State::Running,
                                    Kind::Wait if waiting => State::Running,
                                    Kind::Shot | Kind::Publish if waiting => State::Passed,
                                    _ => State::Pending,
                                },
                                log: if kind == Kind::Build { "Finished release profile".into() } else { String::new() },
                            }).collect(),
                        ..Default::default()
                    }, cx);
                    cx.notify();
                })?;
            }
            Step::WaitEvent { event, timeout_ms } => {
                let count = self.seen.get(&event).copied().unwrap_or(0);
                self.wait_for(cx, &event, timeout_ms.unwrap_or(TIMEOUT_MS), |view, _| {
                    view.shot_activity.as_ref().unwrap().count(&event) > count
                })
                .await?;
                self.seen.insert(event, count + 1);
            }
            Step::SeedApprovalDock => {
                ensure!(!self.read_only, "approval dock needs an isolated fixture");
                self.update(cx, |view, window, cx| view.seed_approval_dock_fixture(window, cx))?;
            }
            Step::SeedIntegrateApproval => {
                ensure!(!self.read_only, "approval dock needs an isolated fixture");
                self.update(cx, |view, window, cx| view.seed_integrate_approval_fixture(window, cx))?;
            }
            Step::SeedCommandApproval => {
                ensure!(!self.read_only, "command approval needs an isolated fixture");
                self.update(cx, |view, _, cx| {
                    view.on_event(ServerEvent::ApprovalRequested {
                        session: view.active.clone(),
                        request_id: "command-fixture".into(),
                        actor: fuzukue_proto::Actor::Claude,
                        summary: long_approval_command(),
                        localized: None, shell_directory: None, shell_prefix: None,
                    }, cx);
                })?;
            }
            Step::SeedComposerContext => self.update(cx, |view, _, cx| seed_composer_context(view, cx))?,
            Step::SeedGoalStatus { reason } => self.update(cx, |view, _, cx| {
                let session = view.active.clone();
                view.threads_loaded = true;
                view.on_event(ServerEvent::ChatWaiting { waiting: fuzukue_proto::ChatWaiting {
                    goal_status: [(session, fuzukue_proto::GoalTail::PendingVerification { reason })].into(),
                    ..Default::default()
                } }, cx);
            })?,
            Step::AssertApprovalDockScrolled => {
                self.wait_for(cx, "approval dock scroll", TIMEOUT_MS, |view, _| view.approval_dock_scrolled()).await?;
            }
            Step::ResolveApprovalDock => {
                ensure!(!self.read_only, "approval dock needs an isolated fixture");
                self.update(cx, |view, window, cx| view.resolve_approval_dock_fixture(window, cx))?;
            }
            Step::ResolveApproval { decision } => {
                ensure!(!self.read_only, "resolving approvals needs an isolated fixture");
                ensure!(matches!(decision.as_str(), "accept" | "decline"), "invalid approval decision");
                let id = self.update(cx, |view, _, _| {
                    view.chats.get(&view.active)?.first_pending_approval().map(str::to_owned)
                })?.context("no pending approval in the active thread")?;
                self.click(cx, ClickTarget::Element { id: format!("{decision}-{id}") })?;
            }
            Step::WaitResearchIssue { number, state } => {
                self.wait_for(cx, &format!("research issue #{number} {state:?}"), TIMEOUT_MS, |view, _| {
                    view.workshop.error.is_some()
                        || view.workshop.issues.iter().any(|issue| issue.number == number && issue.state == state)
                }).await?;
                if let Some(error) = self.update(cx, |view, _, _| view.workshop.error.clone())? {
                    bail!("research issue snapshot failed: {error}");
                }
            }
            Step::WaitGit { path, diff, commit } => {
                self.wait_for(cx, "Git state in the thread information panel", TIMEOUT_MS, |view, _| {
                    let git = &view.thread_info.git;
                    git.status.as_ref().is_some_and(|status| {
                        path.as_ref().is_none_or(|path| status.files.iter().any(|file| &file.path == path))
                            && diff.as_ref().is_none_or(|text| git.diff_contains(text))
                            && commit.as_ref().is_none_or(|subject| status.files.is_empty()
                                && git.commits.first().is_some_and(|commit| &commit.subject == subject))
                    })
                }).await?;
            }
            Step::WaitIdle {
                quiet_ms,
                timeout_ms,
            } => {
                let quiet = Duration::from_millis(quiet_ms.unwrap_or(200));
                let start = Instant::now();
                self.wait_for(
                    cx,
                    "idle chat and quiet connection",
                    timeout_ms.unwrap_or(TIMEOUT_MS),
                    |view, _| {
                        !view.chats[&view.active].working()
                            && view
                                .shot_activity
                                .as_ref()
                                .unwrap()
                                .last
                                .unwrap_or(start)
                                .elapsed()
                                >= quiet
                    },
                )
                .await?;
            }
            Step::OpenWorkspace { path, path_tail } => {
                ensure!(!self.read_only, "open_workspace needs an isolated fixture");
                self.click(cx, ClickTarget::Element { id: "workspace-account".into() })?;
                self.frame(cx)?;
                self.click(cx, ClickTarget::Element { id: "open-workspace".into() })?;
                self.wait_for(cx, "workspace browser home", TIMEOUT_MS, |view, _| view.browsed_workspace_path() == Some("~")).await?;
                // The path is fixture data. Fill it without depending on keyboard focus,
                // then exercise the chooser's Go and Open buttons.
                self.update(cx, |view, window, cx| {
                    let input = view.shot_workspace_path_input().context("workspace browser closed")?;
                    input.update(cx, |input, cx| input.set_value(path.clone(), window, cx));
                    Ok::<_, anyhow::Error>(())
                })??;
                self.frame(cx)?;
                self.click(cx, ClickTarget::Element { id: "workspace-go-to-path".into() })?;
                let description = format!("workspace folder {path:?}");
                if let Err(error) = self.wait_for(cx, &description, TIMEOUT_MS, |view, _| {
                    view.browsed_workspace_path().and_then(|p| p.rsplit('/').next()) == Some(path_tail.as_str())
                }).await {
                    let state = self.update(cx, |view, _, _| view.shot_workspace_browser_state())?;
                    return Err(error.context(state));
                }
                self.frame(cx)?;
                self.click(cx, ClickTarget::Element { id: "confirm-open-workspace".into() })?;
                self.wait_for_workspace(cx, &path_tail).await?;
            }
            Step::SwitchWorkspace { path_tail } => {
                ensure!(!self.read_only, "switch_workspace needs an isolated fixture");
                let id = self.update(cx, |view, _, _| {
                    view.workspaces.entries.iter().find(|entry| entry.path.rsplit('/').next() == Some(&path_tail))
                        .map(|entry| entry.id.clone())
                })?.with_context(|| format!("unknown workspace: {path_tail}"))?;
                self.click(cx, ClickTarget::Element { id: format!("workspace-{id}") })?;
                self.wait_for_workspace(cx, &path_tail).await?;
            }
            Step::SelectThread { title } => {
                self.update(cx, |view, _, cx| {
                    let mut matching = view.threads.iter().filter(|thread| thread.title == title);
                    let id = matching
                        .next()
                        .with_context(|| format!("thread not found: {title}"))?
                        .id
                        .clone();
                    ensure!(matching.next().is_none(), "ambiguous thread title: {title}");
                    view.select_thread(id, cx);
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::SeedThreadSwitch => self.update(cx, |view, _, cx| seed_thread_switch(view, cx))?,
            Step::SeedChatReply { text } => {
                ensure!(!self.read_only, "seed_chat_reply needs an isolated fixture");
                self.update(cx, |view, _, cx| {
                    let session = String::from("chat-reply-fixture");
                    let actor = fuzukue_proto::Actor::Claude;
                    view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                    view.threads_loaded = true;
                    view.switch_session(session.clone(), cx);
                    view.on_event(ServerEvent::TextDelta {
                        session: session.clone(),
                        item: "fixture-reply".into(),
                        actor: actor.clone(),
                        text,
                    }, cx);
                    view.on_event(ServerEvent::TurnEnded { session, actor }, cx);
                })?;
            }
            Step::AssertThreadSwitch { session, restore } => self.assert_thread_switch(cx, &session, restore)?,
            Step::AssertActiveThread { title } => {
                self.update(cx, |view, _, _| {
                    let thread = view.threads.iter().find(|thread| thread.id == view.active)
                        .context("no active thread")?;
                    ensure!(thread.title == title, "active thread is {:?}, expected {title:?}", thread.title);
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::OpenThreadMenu => {
                let id = self.update(cx, |view, _, _| view.active.clone())?;
                ensure!(!id.is_empty(), "no active thread");
                self.click(
                    cx,
                    ClickTarget::Element {
                        id: format!("thread-menu-{id}"),
                    },
                )?;
            }
            Step::HoverThread { title } => {
                self.update(cx, |view, window, cx| {
                    let thread = view.threads.iter().find(|thread| thread.title == title)
                        .with_context(|| format!("thread not found: {title}"))?;
                    let position = window.shot_element_bounds(&format!("thread-row-all-{}", thread.id))
                        .with_context(|| format!("thread is not visible: {title}"))?.center();
                    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::WaitThreadPinned { title } => {
                self.wait_for(cx, "pinned thread", TIMEOUT_MS, |view, _| {
                    view.threads.iter().any(|thread| thread.title == title && thread.pinned)
                }).await?;
            }
            Step::WaitThreadArchived { title } => {
                self.wait_for(cx, "archived thread", TIMEOUT_MS, |view, _| {
                    view.threads
                        .iter()
                        .any(|thread| thread.title == title && thread.archived)
                })
                .await?;
            }
            Step::AssertModel { model } => {
                let actual = self.update(cx, |view, _, _| {
                    view.chats.get(&view.active).and_then(|chat| chat.settings.model.clone())
                })?;
                ensure!(actual.as_deref() == Some(model.as_str()), "expected model {model}, got {actual:?}");
            }
            Step::AssertWorkerModel { model } => {
                self.wait_for(cx, "worker model", TIMEOUT_MS, |view, _| {
                    view.threads.iter().find(|thread| thread.id == view.active)
                        .and_then(|thread| thread.worker_settings.as_ref())
                        .is_some_and(|settings| settings.model == model)
                }).await?;
            }
            Step::AssertDelegateReport { model } => {
                self.wait_for(cx, "completed delegate report", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_delegate_report(&model))
                }).await?;
            }
            Step::OpenDelegateDetails => {
                let id = self.update(cx, |view, _, _| {
                    view.chats.get(&view.active).and_then(|chat| chat.delegate_details_id())
                })?.context("No delegate in the current conversation")?;
                self.click(cx, ClickTarget::Element { id })?;
            }
            Step::WaitDelegateText { text } => {
                self.wait_for(cx, "worker message", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_live_delegate_text(&text))
                }).await?;
            }
            Step::WaitDelegateSummaries { count } => {
                self.wait_for(cx, "concurrent job summaries", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_live_delegate_summaries(count))
                }).await?;
            }
            Step::WaitPartnerText { text } => {
                self.wait_for(cx, "advisor message", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_partner_text(&text))
                }).await?;
            }
            Step::WaitResponding { status } => {
                self.wait_for(cx, "composer stop button and response status at the chat tail", TIMEOUT_MS, |view, window| {
                    window.shot_element_bounds("stop-prompt").is_some()
                        && window.shot_element_bounds("chat-tail-activity-label").is_some()
                        && view.chats.get(&view.active).and_then(|chat| chat.turn_status()).as_deref() == Some(status.as_str())
                }).await?;
            }
            Step::Upload { name, text } => {
                ensure!(!self.read_only, "upload needs an isolated fixture");
                ensure!(!name.starts_with('.') && !name.contains(['/', '\\', ':']) && !name.is_empty(),
                    "upload expects a file name");
                let dir = tempfile::tempdir()?;
                let path = dir.path().join(&name);
                std::fs::write(&path, text)?;
                self.update(cx, |view, _, cx| {
                    ensure!(!view.active.is_empty(), "no active thread");
                    view.add_attachments(view.active.clone(), vec![path], cx);
                    Ok::<_, anyhow::Error>(())
                })??;
                self.wait_for(cx, "attachment upload", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_uploaded_attachment(&name))
                }).await?;
            }
            Step::ClipboardImage => {
                ensure!(!self.read_only, "clipboard image needs an isolated fixture");
                let bitmap = image::RgbaImage::from_fn(360, 240, |x, y| {
                    if x / 40 % 2 == y / 40 % 2 {
                        image::Rgba([30, 118, 190, 255])
                    } else {
                        image::Rgba([222, 234, 242, 255])
                    }
                });
                let mut png = std::io::Cursor::new(Vec::new());
                bitmap.write_to(&mut png, image::ImageFormat::Png)?;
                let bytes = png.into_inner();
                let image = gpui_kit::Image {
                    format: gpui_kit::ImageFormat::Png, id: gpui_kit::hash(&bytes), bytes,
                };
                self.update(cx, |_, _, cx| {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(&image));
                })?;
            }
            Step::AssertOrder { above, below } => {
                self.update(cx, |_, window, _| {
                    let first = window.shot_element_bounds(&above).with_context(|| format!("element is not visible: {above}"))?;
                    let second = window.shot_element_bounds(&below).with_context(|| format!("element is not visible: {below}"))?;
                    ensure!(first.bottom() <= second.top(), "{above} is not above {below}");
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::AssertThreadInfoLayout { open } => {
                self.update(cx, |_, window, _| -> Result<()> {
                    let bounds = |id: &str| window.shot_element_bounds(id)
                        .with_context(|| format!("element is not visible: {id}"));
                    let content_top = bounds("workbench-sidebar")?.top();
                    ensure!((bounds("thread-header")?.top() - content_top).abs() <= px(1.),
                        "a row above the chat header takes up workbench space");
                    if open {
                        let pane = bounds("workbench-right-pane")?;
                        let close = bounds("close-thread-details")?;
                        ensure!((pane.top() - content_top).abs() <= px(1.),
                            "a row above the thread information pane takes up workbench space");
                        ensure!(close.top() >= pane.top() && close.bottom() <= pane.bottom()
                            && close.left() >= pane.left() && close.right() <= pane.right(),
                            "the close button lies outside the thread information pane");
                        let tests = bounds("thread-tests")?;
                        ensure!(tests.right() <= close.left()
                            && (tests.center().y - close.center().y).abs() <= px(1.),
                            "the close button is not beside the Tests button");
                    } else {
                        ensure!(window.shot_element_bounds("workbench-right-pane").is_none()
                            && window.shot_element_bounds("close-thread-details").is_none(),
                            "the thread information pane or its close button is still visible");
                    }
                    Ok(())
                })??;
            }
            Step::WaitElement { id } => {
                self.wait_for(cx, &id, TIMEOUT_MS, |_, window| window.shot_element_bounds(&id).is_some()).await?;
            }
            Step::WaitNote { path, text } => {
                self.wait_for(cx, &path, TIMEOUT_MS, |view, _| {
                    view.right == RightPane::Note && view.note.as_ref()
                        .is_some_and(|(current, body)| current == &path && body.contains(&text))
                }).await?;
            }
            Step::WaitTrashTitle { title } => {
                self.wait_for(cx, &title, TIMEOUT_MS, |view, window| {
                    view.trash_items.iter().flatten().any(|item| {
                        item.title.as_deref() == Some(title.as_str())
                            && window.shot_element_bounds(&format!("trash-item-title-{}", item.id)).is_some()
                    })
                }).await?;
            }
            Step::WaitInboxProposal { to } => {
                self.wait_for(cx, "inbox proposal", TIMEOUT_MS, |view, _| {
                    view.chats.get(&view.active).is_some_and(|chat| chat.has_inbox_proposal(&to))
                }).await?;
            }
            Step::WaitReviews { deck, kinds, running } => {
                self.wait_for(cx, "review status", TIMEOUT_MS, |view, _| {
                    kinds.iter().all(|kind| view.review_jobs.get(&(deck.clone(), kind.clone())).is_some_and(|job| {
                        if running { job.started_at.is_some() } else { job.count.is_some() }
                    }))
                }).await?;
            }
            Step::StartComment { card, quote } => {
                ensure!(!self.read_only, "start_comment needs an isolated fixture");
                self.update(cx, |view, window, cx| {
                    ensure!(view.right == RightPane::Read, "paper view is closed");
                    ensure!(view.cards.iter().any(|item| item.id == card), "card not found: {card}");
                    view.start_comment(super::CommentTarget { card, quote, source: None, proof: false }, window, cx);
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::NewThread { deck } => {
                ensure!(
                    !self.read_only,
                    "new_thread requires a local fixture server"
                );
                let active = self.update(cx, |view, _, _| view.active.clone())?;
                if let Some(deck) = &deck {
                    self.update(cx, |view, _, _| {
                        view.out.send(fuzukue_proto::ClientMsg::NewAttachedThread { deck: deck.clone() })
                    })??;
                } else {
                    self.click(
                        cx,
                        ClickTarget::Element {
                            id: "new-thread".into(),
                        },
                    )?;
                }
                self.wait_for(cx, "new thread", TIMEOUT_MS, |view, _| {
                    view.active != active
                        && view.threads.iter().any(|thread| {
                            thread.id == view.active
                                && deck.as_ref().is_none_or(|deck| thread.attached.contains(deck))
                        })
                })
                .await?;
            }
            Step::AttachDeck { deck } => {
                ensure!(
                    !self.read_only,
                    "attach_deck requires a local fixture server"
                );
                self.update(cx, |view, _, _| {
                    ensure!(!view.active.is_empty(), "no active thread");
                    view.out.send(ClientMsg::AttachThread {
                        id: view.active.clone(),
                        decks: vec![deck.clone()],
                    })?;
                    Ok::<_, anyhow::Error>(())
                })??;
                self.wait_for(cx, "thread attachment", TIMEOUT_MS, |view, _| {
                    view.threads.iter().any(|thread| {
                        thread.id == view.active && thread.attached.iter().any(|path| path == &deck)
                    })
                })
                .await?;
            }
            Step::OpenPaper { deck, via } => {
                let document = self.update(cx, |view, _, cx| {
                    ensure!(
                        !self.read_only || view.deck == deck,
                        "a read-only connection can open only the server's current deck ({})",
                        view.deck
                    );
                    ensure!(view.paper_deck(&deck), "paper deck not found: {deck}");
                    let count = view.shot_activity.as_ref().unwrap().count("document");
                    if via.is_none() {
                        view.open_paper_deck(deck.clone(), cx);
                    }
                    Ok::<_, anyhow::Error>(if view.deck == deck { count } else { count + 1 })
                })??;
                if let Some(id) = via {
                    self.click(cx, ClickTarget::Element { id })?;
                }
                self.wait_for(
                    cx,
                    &format!("document for {deck}"),
                    TIMEOUT_MS,
                    |view, _| {
                        view.right == RightPane::Read
                            && view.deck == deck
                            && view.shot_activity.as_ref().unwrap().count("document") >= document
                    },
                )
                .await?;
            }
            Step::BuildPaper => {
                ensure!(!self.read_only, "build_paper requires a local fixture server");
                let count = self.update(cx, |view, _, _| {
                    ensure!(view.right == RightPane::Read, "paper view is closed");
                    view.out.send(fuzukue_proto::ClientMsg::BuildPaper { deck: view.deck.clone() })?;
                    Ok::<_, anyhow::Error>(view.shot_activity.as_ref().unwrap().count("build_result"))
                })??;
                self.wait_for(cx, "paper build", 90_000, |view, _| {
                    view.shot_activity.as_ref().unwrap().count("build_result") > count
                }).await?;
                self.update(cx, |view, _, _| {
                    ensure!(view.built_pdfs.contains_key(&view.deck), "paper build failed");
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::WaitPdf => {
                self.wait_for_with_context(cx, "first PDF page", TIMEOUT_MS, |view, _, cx| {
                    (view.paper_pdf_visible() || view.right == RightPane::Pdf)
                        && view.pdf.as_ref().is_some_and(|(_, pdf)| pdf.read(cx).page_ready(0))
                }).await?;
            }
            Step::ClickPdfCard { card, ctrl } => {
                self.wait_for(cx, "PDF card map", TIMEOUT_MS, |view, _| view.pdf_regions().is_some()).await?;
                let (page, x, y) = self.update(cx, |view, _, cx| {
                    ensure!(view.paper_pdf_visible() || (ctrl && view.right == RightPane::Pdf), "paper PDF is closed");
                    let region = view.pdf_regions().unwrap().iter()
                        .filter(|region| region.card == card)
                        .min_by_key(|region| (region.page, region.proof))
                        .with_context(|| format!("card has no PDF region: {card}"))?;
                    let [left, top, width, height] = region.rect;
                    let point = (region.page, left + width * 0.33, top + height * 0.22);
                    let (_, pdf) = view.pdf.as_ref().context("PDF is unavailable")?;
                    pdf.update(cx, |pdf, cx| pdf.reveal_position(point.0, point.2, cx));
                    Ok::<_, anyhow::Error>(point)
                })??;
                self.wait_for_with_context(cx, "PDF target page", TIMEOUT_MS, |view, _, cx| {
                    view.pdf.as_ref().is_some_and(|(_, pdf)| pdf.read(cx).page_ready(page))
                }).await?;
                let position = self.update(cx, |view, window, cx| {
                    let (_, pdf) = view.pdf.as_ref().context("PDF is unavailable")?;
                    let point = pdf.read(cx).page_screen_bounds(page, (x, y, 0., 0.))
                        .context("PDF page is not laid out")?.origin;
                    ensure!(window.shot_element_bounds("pdf-scroll").is_some_and(|bounds| bounds.contains(&point)),
                        "PDF point is outside the visible page");
                    Ok::<_, anyhow::Error>(point)
                })??;
                if ctrl {
                    self.ctrl_click_pdf(cx, position)?;
                    self.wait_for(cx, "PDF card jump", TIMEOUT_MS, |view, _| {
                        !view.paper_pdf_visible() && view.right == RightPane::Read
                            && view.paper_jump.as_ref().is_some_and(|jump| jump.card == card && !jump.pending)
                    }).await?;
                } else {
                    self.click(cx, ClickTarget::Coordinates { x: position.x.into(), y: position.y.into() })?;
                    self.wait_for(cx, "PDF card popup", TIMEOUT_MS, |view, _| {
                        view.paper_pdf.popup.as_ref().is_some_and(|popup| popup.card == card)
                    }).await?;
                }
            }
            Step::Scroll { pane, pixels } => {
                self.update(cx, |view, _, cx| {
                    let list = match pane {
                        Pane::Paper => {
                            ensure!(view.right == RightPane::Read, "paper view is closed");
                            &view.read_list
                        }
                        Pane::Chat => &view.chats[&view.active].scroller,
                        Pane::ApprovalDock => bail!("use the wheel action for the approval dock"),
                    };
                    list.scroll_by(px(pixels));
                    cx.notify();
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::Wheel { pane, pixels, lines, pages } => {
                use gpui_kit::{ScrollDelta, ScrollEvent};
                self.update(cx, |_, window, cx| {
                    let id = match pane {
                        Pane::Paper => "paper-centre-column",
                        Pane::Chat => "paper-chat-column",
                        Pane::ApprovalDock => "approval-dock-scroll",
                    };
                    let position = window.shot_element_bounds(id)
                        .map(|b| b.center())
                        .unwrap_or_else(|| point(window.viewport_size().width / 2., window.viewport_size().height / 2.));
                    let delta = match (pixels, pages, lines) {
                        (Some(p), _, _) => ScrollDelta::Pixels(point(px(0.), px(p))),
                        (_, Some(p), _) => ScrollDelta::Pages(point(0., p)),
                        (_, _, l) => ScrollDelta::Lines(point(0., l.unwrap_or(-3.))),
                    };
                    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                    window.dispatch_event(PlatformInput::Scroll(ScrollEvent { position, delta, ..Default::default() }), cx);
                })?;
            }
            Step::LogScroll => {
                self.update(cx, |view, _, _| {
                    let paper = view.read_list.logical_scroll_top();
                    let chat = view.chats.get(&view.active).map(|c| c.scroller.logical_scroll_top());
                    eprintln!("scroll: paper={paper:?} chat={chat:?}");
                })?;
            }
            Step::ScrollToCard { id } => {
                self.update(cx, |view, _, cx| {
                    ensure!(view.right == RightPane::Read, "paper view is closed");
                    ensure!(
                        view.read_rows
                            .iter()
                            .any(|row| matches!(row, ReadRow::Card(card) if card == &id)),
                        "paper card not found: {id}"
                    );
                    view.scroll_paper_to_card(id, false, cx);
                    Ok::<_, anyhow::Error>(())
                })??;
                self.wait_for(cx, "paper scroll settled", TIMEOUT_MS, |view, _| {
                    view.paper_jump.as_ref().is_some_and(|jump| !jump.pending)
                })
                .await?;
            }
            Step::WaitPaperCard { deck, card, text } => {
                self.wait_for(cx, "target paper card", TIMEOUT_MS, |view, _| {
                    view.right == RightPane::Read
                        && view.deck == deck
                        && view.paper_jump.as_ref().is_some_and(|jump| jump.card == card && !jump.pending)
                        && text.as_ref().is_none_or(|text| {
                            view.card_editor(&card, false).is_none()
                                && view.read_md.get(&super::ReadPart { card: card.clone(), proof: false })
                                    .is_some_and(|part| part.display.markdown.contains(text))
                        })
                }).await?;
            }
            Step::ExpandProofs => {
                self.update(cx, |view, _, cx| {
                    ensure!(view.right == RightPane::Read, "paper view is closed");
                    view.open_proofs
                        .entry(view.deck.clone())
                        .or_default()
                        .extend(
                            view.read_md
                                .keys()
                                .filter(|part| part.proof)
                                .map(|part| part.card.clone()),
                        );
                    view.read_list.remeasure_items(0..view.read_rows.len());
                    cx.notify();
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::Click { id, x, y } => {
                let target = match (id, x, y) {
                    (Some(id), None, None) => ClickTarget::Element { id },
                    (None, Some(x), Some(y)) => ClickTarget::Coordinates { x, y },
                    _ => bail!("click expects either id or both x and y"),
                };
                self.click(cx, target)?;
            }
            Step::CtrlClickText { card, text } => {
                let position = self.update(cx, |this, window, cx| {
                    let body = &this.cards.iter().find(|entry| entry.id == card)
                        .with_context(|| format!("unknown card: {card}"))?.body;
                    let part = super::ReadPart { card: card.clone(), proof: false };
                    let read = this.read_md.get(&part).context("missing rendered card")?;
                    let view = read.view.read(cx);
                    let markdown_ranges: Vec<_> = body.match_indices(&text)
                        .filter_map(|(start, value)| read.display.markdown_range(start..start + value.len()))
                        .collect();
                    let bounds = window.shot_element_bounds(&format!("symbol-text-{card}-false"))
                        .context("card body is outside the viewport")?
                        .intersect(&window.visual_viewport_bounds());
                    let mut found = None;
                    'rows: for y in (f32::from(bounds.top()).ceil() as i32..f32::from(bounds.bottom()) as i32).step_by(3) {
                        for x in (f32::from(bounds.left()).ceil() as i32..f32::from(bounds.right()) as i32).step_by(3) {
                            let point = point(px(x as f32), px(y as f32));
                            if view.source_range_at(point).is_some_and(|hit| markdown_ranges.iter()
                                .any(|range| range.start < hit.end && hit.start < range.end))
                            {
                                found = Some(point);
                                break 'rows;
                            }
                        }
                    }
                    found.with_context(|| format!("text is not visible in {card}: {text}"))
                })??;
                cx.update_window(self.handle, |_, window, cx| {
                    let modifiers = gpui_kit::Modifiers { control: true, ..Default::default() };
                    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                        position, modifiers, ..Default::default()
                    }), cx);
                    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                        position, modifiers, click_count: 1, ..Default::default()
                    }), cx);
                    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                        position, modifiers, click_count: 1, ..Default::default()
                    }), cx);
                })?;
            }
            Step::HoverSource { line, col } => {
                use gpui_kit::component::input::{Position, RopeExt as _};
                ensure!(line > 0 && col > 0, "source line and column must be 1-based");
                self.update(cx, |view, window, cx| {
                    let editor = view.source.active_editor().context("source editor is closed")?.read(cx);
                    let start = editor.text().position_to_offset(&Position::new(line - 1, col - 1));
                    let end = editor.text().position_to_offset(&Position::new(line - 1, col));
                    let position = editor.range_to_bounds(&(start..end)).context("source position is not visible")?.center();
                    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::WaitSourceDiagnostics => {
                self.wait_for_with_context(cx, "source diagnostics", TIMEOUT_MS, |view, _, cx| {
                    view.source.active_editor().and_then(|editor| editor.read(cx).diagnostics())
                        .is_some_and(|diagnostics| !diagnostics.is_empty())
                }).await?;
            }
            Step::WaitSourceHover => {
                self.wait_for_with_context(cx, "source hover", TIMEOUT_MS, |view, _, cx| {
                    view.source.active_editor().is_some_and(|editor| editor.read(cx).hover_popover().is_some())
                }).await?;
            }
            Step::Hover { id } => self.hover_element(cx, &id)?,
            Step::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                self.drag(cx, point(px(from_x), px(from_y)), point(px(to_x), px(to_y)))?;
            }
            Step::DragFromElement { id, dx, dy } => {
                let from = self.update(cx, |_, window, _| {
                    window.shot_element_bounds(&id).map(|bounds| bounds.center())
                })?.with_context(|| format!("drag start element is not visible: {id}"))?;
                self.drag(cx, from, from + point(px(dx), px(dy)))?;
            }
            Step::AssertCardScrollbarDrag => self.assert_card_scrollbar_drag(cx).await?,
            Step::AssertPaneTextSelection => self.assert_pane_text_selection(cx).await?,
            Step::AssertTranscriptScrollGrowth => self.assert_transcript_scroll_growth(cx)?,
            Step::AssertThreadNavigationKeys => self.assert_thread_navigation_keys(cx).await?,
            Step::AssertChatReplayLoad => self.assert_chat_replay_load(cx)?,
            Step::AssertThreadHistoryPaging => self.assert_thread_history_paging(cx)?,
            Step::AssertSidebarWidth { width } => {
                let settings = fuzukue_proto::compat::env_os("FUZUKUE_SHOT_SETTINGS")
                    .context("screenshot settings path is missing")?;
                let saved = AppearanceSettings::from_toml(&std::fs::read_to_string(settings)?)?
                    .sidebar_width;
                self.update(cx, |_, window, _| {
                    let bounds = window
                        .shot_element_bounds("workspace-sidebar")
                        .context("sidebar is not visible")?;
                    let actual = f32::from(bounds.size.width);
                    ensure!(
                        (actual - width).abs() < 1. && (saved - width).abs() < 1.,
                        "expected sidebar width {width}, got rendered {actual} and saved {saved}"
                    );
                    Ok::<_, anyhow::Error>(())
                })??;
            }
            Step::AssertFileTreeLabels { paths } => self.assert_file_tree_labels(cx, &paths)?,
            Step::Type { text } => self.type_text(cx, &text)?,
            Step::AssertComposerText { text } => {
                let actual = self.update(cx, |view, _, cx| view.input.read(cx).value())?;
                ensure!(actual.as_ref() == text, "expected composer text {text:?}, got {actual:?}");
            }
            Step::AssertComposerDecorations { styled, dimmed } => {
                let counts = self.update(cx, |view, _, cx| {
                    let decorations = view.input.read(cx).decorations();
                    let dimmed = decorations.iter().filter(|item| item.style.fade_out.is_some()).count();
                    (decorations.len() - dimmed, dimmed)
                })?;
                ensure!(counts == (styled, dimmed),
                    "expected {styled} styled and {dimmed} dimmed composer spans, got {counts:?}");
            }
            Step::Key { key } => self.key(cx, &key)?,
            Step::Wait { ms } => pause(cx, ms).await,
            Step::Screenshot { name } => self.screenshot(cx, &name).await?,
        }
        Ok(())
    }

    fn update<R>(
        &self,
        cx: &mut AsyncApp,
        action: impl FnOnce(&mut Workbench, &mut Window, &mut Context<Workbench>) -> R,
    ) -> Result<R> {
        self.handle.update(cx, |_, window, cx| {
            self.view.update(cx, |view, cx| action(view, window, cx))
        })
    }

    fn frame(&self, cx: &mut AsyncApp) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.present_if_needed();
        })
    }

    fn assert_thread_switch(&self, cx: &mut AsyncApp, session: &str, restore: bool) -> Result<()> {
        let list = self.update(cx, |view, _, _| {
            view.chats.get(session).map(|chat| chat.scroller.clone())
        })?.with_context(|| format!("unknown thread {session}"))?;
        let position = || (
            list.frame_stats().first_painted,
            list.logical_scroll_top(),
            list.scroll_px_offset_for_scrollbar().y,
        );
        let saved = position();
        let transcript = format!("chat-{session}");
        let last_row = list.item_count().checked_sub(1).context("empty transcript")?;
        let tail = format!("chat-message-bubble-{last_row}");
        self.click(cx, ClickTarget::Element { id: format!("thread-row-title-{session}") })?;
        // Capture the first draw after the click, without a wait step that can paint over it.
        let mut frames = Vec::with_capacity(11);
        for frame in 0..11 {
            self.frame(cx)?;
            let current = position();
            let (first, anchor, offset) = current;
            let (switched, head, tail_visible) = self.update(cx, |view, window, _| {
                let viewport = list.viewport_bounds();
                let visible = |id| window.shot_element_bounds(id).is_some_and(|bounds| bounds.intersects(&viewport));
                (
                    view.active == session && window.shot_element_bounds(&transcript).is_some(),
                    visible("chat-message-bubble-0"),
                    visible(tail.as_str()),
                )
            })?;
            eprintln!("thread switch {session} frame {frame}: first={first:?} anchor={anchor:?} offset={offset}");
            let at_tail = tail_visible && (list.max_offset_for_scrollbar().y + offset).abs() <= px(1.);
            frames.push((current, switched, head, at_tail));
        }
        let expected = if restore { saved } else { frames[0].0 };
        for (frame, ((first, anchor, offset), switched, head, at_tail)) in frames.into_iter().enumerate() {
            ensure!(switched && first.is_some() && !head, "{session} frame {frame} did not display the expected transcript position");
            if !restore {
                ensure!(at_tail && first.is_some_and(|row| row >= last_row.saturating_sub(30)) && offset < px(0.),
                    "{session} frame {frame} did not open at the tail");
            }
            ensure!(first == expected.0 && anchor == expected.1 && (offset - expected.2).abs() <= px(1.),
                "{session} frame {frame} did not keep the expected position {expected:?}");
        }
        Ok(())
    }

    fn assert_file_tree_labels(&self, cx: &mut AsyncApp, paths: &[String]) -> Result<()> {
        self.update(cx, |_, window, _| {
            for path in paths {
                let label = window
                    .shot_element_bounds(&format!("file-tree-label-{path}"))
                    .with_context(|| format!("folder label is not visible for {path:?}"))?;
                ensure!(
                    label.size.width >= window.rem_size() * 2.5,
                    "folder label for {path:?} is only {} wide",
                    label.size.width
                );
            }
            Ok(())
        })?
    }

    async fn wait_for(
        &self,
        cx: &mut AsyncApp,
        description: &str,
        timeout_ms: u64,
        ready: impl Fn(&Workbench, &Window) -> bool,
    ) -> Result<()> {
        self.wait_for_with_context(cx, description, timeout_ms, |view, window, _| ready(view, window)).await
    }

    async fn wait_for_with_context(
        &self,
        cx: &mut AsyncApp,
        description: &str,
        timeout_ms: u64,
        ready: impl Fn(&Workbench, &Window, &Context<Workbench>) -> bool,
    ) -> Result<()> {
        let start = Instant::now();
        loop {
            self.frame(cx)?;
            let (ready, status) = self.update(cx, |view, window, cx| {
                (ready(view, window, cx), view.status.text().into_owned())
            })?;
            if ready {
                return Ok(());
            }
            ensure!(
                start.elapsed() < Duration::from_millis(timeout_ms),
                "timed out waiting for {description} ({status})"
            );
            pause(cx, 30).await;
        }
    }

    async fn wait_for_workspace(&self, cx: &mut AsyncApp, path_tail: &str) -> Result<()> {
        self.wait_for(cx, path_tail, TIMEOUT_MS, |view, _| {
            view.threads_loaded && view.workspaces.entries.iter().any(|entry| {
                Some(&entry.id) == view.workspaces.active.as_ref()
                    && entry.path.rsplit('/').next() == Some(path_tail)
            })
        }).await
    }

    fn type_text(&self, cx: &mut AsyncApp, text: &str) -> Result<()> {
        let keystroke = Keystroke {
            key: "unidentified".into(),
            key_char: Some(text.into()),
            modifiers: Default::default(),
        };
        let handled = cx.update_window(self.handle, |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx)
        })?;
        ensure!(handled, "text was not accepted by a focused input");
        Ok(())
    }

    fn key(&self, cx: &mut AsyncApp, key: &str) -> Result<()> {
        let key = Keystroke::parse_with_os(
            &key.to_ascii_lowercase().replace('+', "-"),
            gpui_kit::OperatingSystem::Linux,
        )
        .map_err(|err| anyhow!("{err}"))?;
        cx.update_window(self.handle, |_, window, cx| {
            window.dispatch_keystroke(key, cx)
        })?;
        Ok(())
    }

    #[cfg(test)]
    fn click_menu(&self, cx: &mut AsyncApp, menu: &str, item: &str) -> Result<()> {
        self.frame(cx)?;
        self.click(cx, ClickTarget::Element { id: menu.into() })?;
        self.frame(cx)?;
        self.click(cx, ClickTarget::Element { id: item.into() })
    }

    fn ctrl_click_pdf(&self, cx: &mut AsyncApp, position: gpui_kit::Point<gpui_kit::Pixels>) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            let mac = cx.platform_info().is_mac();
            let modifiers = gpui_kit::Modifiers { control: !mac, platform: mac, ..Default::default() };
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                position, modifiers, ..Default::default()
            }), cx);
            window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                position, modifiers, click_count: 1, ..Default::default()
            }), cx);
            window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                position, modifiers, click_count: 1, ..Default::default()
            }), cx);
        })?;
        Ok(())
    }

    fn click(&self, cx: &mut AsyncApp, target: ClickTarget) -> Result<()> {
        // Older scenarios select a title-bar group directly.
        if matches!(&target, ClickTarget::Element { id } if matches!(
            id.as_str(), "menu-file" | "menu-edit" | "menu-view" | "menu-thread" | "menu-help"
        )) && !self.update(cx, |view, _, _| view.app_menu.is_expanded())? {
            self.click_target(cx, ClickTarget::Element { id: crate::menu_bar::MENU_BUTTON.into() })?;
            self.frame(cx)?;
        }
        self.click_target(cx, target)
    }

    fn hover_element(&self, cx: &mut AsyncApp, id: &str) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            let position = window.shot_element_bounds(id)
                .with_context(|| format!("element is not visible: {id}"))?
                .center();
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }),
                cx,
            );
            Ok(())
        })?
    }

    fn click_target(&self, cx: &mut AsyncApp, target: ClickTarget) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            let position = match target {
                ClickTarget::Element { id } => window
                    .shot_element_bounds(&id)
                    .with_context(|| format!("element is not visible: {id}"))?
                    .center(),
                ClickTarget::Coordinates { x, y } => point(px(x), px(y)),
            };
            ensure!(
                window.visual_viewport_bounds().contains(&position),
                "click is outside the window"
            );
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            Ok::<_, anyhow::Error>(())
        })?
    }

    fn drag(
        &self,
        cx: &mut AsyncApp,
        from: gpui_kit::Point<gpui_kit::Pixels>,
        to: gpui_kit::Point<gpui_kit::Pixels>,
    ) -> Result<()> {
        cx.update_window(self.handle, |_, window, cx| {
            let viewport = window.visual_viewport_bounds();
            ensure!(
                viewport.contains(&from) && viewport.contains(&to),
                "drag is outside the window"
            );
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: from,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    position: from,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            for step in 1..=4 {
                let position = from + (to - from) * (step as f32 / 4.);
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(gpui_kit::MouseButton::Left),
                        ..Default::default()
                    }),
                    cx,
                );
                window.draw(cx).clear(cx);
            }
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    position: to,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            Ok::<_, anyhow::Error>(())
        })?
    }

    async fn screenshot(&self, cx: &mut AsyncApp, name: &str) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "screenshot name must use letters, digits, hyphens or underscores"
        );
        // Let async Markdown layouts and the X11 present run before reading the window.
        for _ in 0..3 {
            self.frame(cx)?;
            pause(cx, 35).await;
        }
        let (xid, expected) = cx.update_window(self.handle, |_, window, _| {
            let raw = window.window_handle()?.as_raw();
            let id = match raw {
                RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
                RawWindowHandle::Xlib(handle) => handle.window as u64,
                _ => bail!("capture requires the isolated X11 window"),
            };
            let size = window.viewport_size();
            Ok::<_, anyhow::Error>((
                id,
                (f32::from(size.width) as u32, f32::from(size.height) as u32),
            ))
        })??;
        let captured = crate::screenshot::capture_x11(xid)?;
        let pixels =
            image::load_from_memory_with_format(&captured, image::ImageFormat::Png)?
                .into_rgb8();
        ensure!(
            pixels.dimensions() == expected,
            "capture dimensions {:?} differ from the window {:?}",
            pixels.dimensions(),
            expected
        );
        let first = pixels
            .pixels()
            .next()
            .ok_or_else(|| anyhow!("empty image"))?;
        ensure!(
            pixels.pixels().any(|pixel| pixel != first),
            "blank screenshot: {name}"
        );
        let path = self.output.join(format!("{name}.png"));
        let mut file = tempfile::NamedTempFile::new_in(&self.output)?;
        file.write_all(&captured)?;
        file.persist(&path)?;
        println!("{}", path.display());
        Ok(())
    }
}

async fn pause(cx: &AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

fn long_approval_command() -> String {
    format!("Bash: printf '%s\\n' \\\n{}", (1..=18).map(|line| {
        format!("  'Draft section {line} for the workspace' \\")
    }).collect::<Vec<_>>().join("\n"))
}

fn screenshot_fixture_png() -> Result<Vec<u8>> {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        320, 180, image::Rgba([30, 118, 190, 255]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)?;
    Ok(png.into_inner())
}

fn seed_github_report(view: &mut Workbench, login: Option<&str>, cx: &mut Context<Workbench>) {
    use fuzukue_proto::{GitHubReportStatus, GitHubReportUpdate, WorkspaceKind};
    view.protocol = fuzukue_proto::GITHUB_REPORT_PROTOCOL;
    view.status = crate::ConnectionStatus::Connected;
    view.on_event(ServerEvent::GitHubReportResult {
        request: 0,
        result: Ok(GitHubReportUpdate::Status { status: GitHubReportStatus {
            enabled: true,
            login: login.map(str::to_owned),
            report_repo: Some("example/project".into()),
            workspace_kind: WorkspaceKind::Research,
        }}),
    }, cx);
}

#[cfg(test)]
#[path = "shot/card_markdown_tests.rs"]
mod card_markdown_tests;

#[cfg(test)]
#[path = "shot/xml_components_test.rs"]
mod xml_components_test;

#[cfg(test)]
#[path = "shot/issue_intake_test.rs"]
mod issue_intake_test;

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Pixels;
    use fuzukue_proto::{ClientMsg, ThreadView};

    #[test]
    fn appearance_font_weights_apply_and_save_from_the_settings_pane() {
        use fuzukue_view::appearance::FontWeightChoice::{Light, Medium, Regular};
        use gpui_kit::FontWeight;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 1200.).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    let dir = tempfile::tempdir()?;
                    let path = dir.path().join("settings.toml");
                    runner.update(cx, |view, _, _| view.appearance.isolate_shot_settings(path.clone()))?;
                    runner.click_menu(cx, "menu-help", "tool-settings")?;
                    for id in ["ui-weight-light", "reading-weight-medium", "code-weight-medium"] {
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: id.into() })?;
                    }
                    runner.update(cx, |view, _, cx| {
                        assert_eq!(view.appearance.ui_font.weight, FontWeight::LIGHT);
                        let reading = view.appearance.style_reading(gpui_kit::StyleRefinement::default());
                        assert_eq!(reading.text.font_weight, Some(FontWeight::MEDIUM));
                        assert_eq!(crate::appearance::code_font(cx).weight, FontWeight::MEDIUM);
                    })?;
                    let saved = AppearanceSettings::from_toml(&std::fs::read_to_string(&path)?)?;
                    assert_eq!((saved.ui_font_weight, saved.reading_font_weight, saved.code_font_weight), (Light, Medium, Medium));

                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "ui-weight-regular".into() })?;
                    let saved = AppearanceSettings::from_toml(&std::fs::read_to_string(&path)?)?;
                    assert_eq!((saved.ui_font_weight, saved.reading_font_weight, saved.code_font_weight), (Regular, Medium, Medium));
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn thread_switch_paints_the_tail_first_and_keeps_anchors() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1200., 760.).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = async {
                    const A: &str = "thread-switch-a";
                    const B: &str = "thread-switch-b";
                    runner.update(cx, |view, _, cx| {
                        seed_thread_switch(view, cx);
                    })?;
                    runner.update(cx, |view, window, cx| {
                        view.input.update(cx, |input, cx| input.set_value("Draft for A", window, cx));
                        view.record_composer_text(cx);
                    })?;
                    runner.frame(cx)?;
                    let snapshot = |cx: &mut AsyncApp| runner.update(cx, |view, window, _| {
                        let chat = &view.chats[&view.active];
                        let scroller = &chat.scroller;
                        let stats = scroller.frame_stats();
                        let visible_top = stats.first_painted.and_then(|row| {
                            window.shot_element_bounds(&format!("chat-message-bubble-{row}"))
                        }).map(|bounds| bounds.top());
                        (stats, scroller.scroll_px_offset_for_scrollbar().y,
                            scroller.logical_scroll_top(), scroller.viewport_bounds(), visible_top,
                            chat.transcript_parse_requests())
                    });
                    assert_eq!(runner.update(cx, |view, _, _| view.chats[B].transcript_parse_requests())?, 0);
                    let a = snapshot(cx)?;
                    runner.assert_thread_switch(cx, B, false)?;
                    let first = snapshot(cx)?;
                    assert!(runner.update(cx, |view, window, cx| {
                        view.active == B && window.shot_element_bounds("chat-message-bubble-299").is_some()
                            && window.shot_element_bounds("chat-message-bubble-0").is_none()
                            && window.shot_element_bounds("thread-header-title").is_some()
                            && view.input.read(cx).value().is_empty()
                    })?);
                    assert!(first.0.first_painted.unwrap() > 280, "{:?}", first.0);
                    assert!(first.0.painted <= first.0.laid_out && first.0.laid_out <= 24, "{:?}", first.0);
                    assert!(first.5 > 0 && first.5 <= first.0.painted);

                    runner.update(cx, |view, _, cx| {
                        view.chats[B].scroller.scroll_to(gpui_kit::ListOffset {
                            item_ix: 137,
                            offset_in_item: px(9.),
                        });
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    let disclosure_height = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| {
                        view.chats[B].scroller.bounds_for_item(139).unwrap().size.height
                    });
                    let closed = disclosure_height(cx)?;
                    runner.click(cx, ClickTarget::Element { id: format!("chat-{B}-139-Json") })?;
                    runner.frame(cx)?;
                    let open = disclosure_height(cx)?;
                    assert!(open > closed);
                    let saved = snapshot(cx)?;
                    assert_eq!(saved.2.item_ix, 137);
                    assert!(saved.5 > first.5);
                    assert!(saved.0.measured > saved.0.painted);
                    for (id, expected) in [(A, a), (B, saved)] {
                        runner.assert_thread_switch(cx, id, true)?;
                        let restored = snapshot(cx)?;
                        assert_eq!(restored.2, expected.2);
                        assert!((restored.1 - expected.1).abs() <= px(1.));
                        assert!((restored.4.unwrap() - expected.4.unwrap()).abs() <= px(1.));
                        assert_eq!(restored.5, expected.5);
                        assert!(runner.update(cx, |view, _, cx| {
                            view.input.read(cx).value().as_str() == if id == A { "Draft for A" } else { "" }
                        })?);
                    }
                    assert_eq!(disclosure_height(cx)?, open);

                    // The in-process headless window has a fixed platform size.
                    // Opening the real side pane exercises a narrower list viewport.
                    runner.update(cx, |view, _, cx| {
                        view.settings_open = true;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    let narrow = snapshot(cx)?;
                    assert!(narrow.3.size.width < saved.3.size.width);
                    assert_eq!(narrow.2, saved.2);
                    assert_eq!(narrow.0.measured, narrow.0.painted);
                    assert_eq!(narrow.5, saved.5);

                    // Replay can extend both histories while keeping the reader's anchor.
                    runner.update(cx, |view, _, cx| {
                        for session in [A, B] {
                            for index in 300..310 {
                                view.on_event(ServerEvent::TextDelta {
                                    session: session.into(),
                                    item: format!("user:{index}"),
                                    actor: fuzukue_proto::Actor::User,
                                    text: format!("More history {index}"),
                                }, cx);
                            }
                        }
                    })?;
                    runner.frame(cx)?;
                    assert_eq!(snapshot(cx)?.2, narrow.2);
                    runner.assert_thread_switch(cx, A, false)?;
                    assert!(runner.update(cx, |view, window, _| {
                        view.chats[A].scroller.is_following_tail()
                            && window.shot_element_bounds("chat-message-bubble-309").is_some()
                    })?);
                    runner.update(cx, |view, _, cx| {
                        for index in 0..12 {
                            view.select_thread(format!("other-history-{index}"), cx);
                        }
                        assert_eq!(view.recent_transcripts.len(), 12);
                        assert!(view.chats[B].scroller.is_following_tail());
                        assert_eq!(view.chats[B].scroller.item_count(), 310);
                    })?;
                    Ok::<_, anyhow::Error>(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn large_thread_replay_and_switch_paint_only_the_viewport() {
        use fuzukue_proto::{Actor, Envelope, PROTOCOL_VERSION};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1200., 760.).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    for (session, count) in [("thread-fixture-tail", 85), ("thread-fixture-full", 20_000)] {
                        let start = Instant::now();
                        runner.update(cx, |view, _, cx| -> Result<()> {
                            view.protocol = PROTOCOL_VERSION;
                            view.receiving_backlog = true;
                            view.status = crate::ConnectionStatus::Connected;
                            let reply = "**Synthetic Markdown** with `inline code`.\n\n".repeat(23);
                            let output = "Synthetic tool output with a workspace path.\n".repeat(11);
                            for turn in 0..count {
                                let tool = format!("tool:{turn}");
                                for event in [
                                    ServerEvent::TextDelta { session: session.into(), item: format!("user:{turn}"),
                                        actor: Actor::User, text: "How should we investigate this fixture? ".repeat(3) },
                                    ServerEvent::TextDelta { session: session.into(), item: format!("reply:{turn}"),
                                        actor: Actor::Claude, text: reply.clone() },
                                    ServerEvent::ToolCall { session: session.into(), item: tool.clone(), actor: Actor::Claude,
                                        name: "Read".into(), input: if turn + 2_000 >= count {
                                            serde_json::json!({"file_path": "notes/synthetic.md"})
                                        } else { serde_json::json!({}) } },
                                    ServerEvent::ToolResult { session: session.into(), item: tool, ok: true,
                                        output: if turn + 2_000 >= count { output.clone() } else { String::new() } },
                                    ServerEvent::TurnEnded { session: session.into(), actor: Actor::Claude },
                                ] {
                                    let frame = serde_json::to_vec(&Envelope { workspace: None, seq: 0, event })?;
                                    let frame: Envelope = serde_json::from_slice(&frame)?;
                                    view.on_event(frame.event, cx);
                                }
                            }
                            assert_eq!(view.chats[session].transcript_parse_requests(), 0);
                            view.receiving_backlog = false;
                            view.select_thread(session.into(), cx);
                            Ok(())
                        })??;
                        runner.frame(cx)?;
                        let first_frame_ms = start.elapsed().as_millis();
                        let stats = runner.update(cx, |view, window, _| {
                            let chat = &view.chats[session];
                            let stats = chat.scroller.frame_stats();
                            let rows = chat.scroller.item_count();
                            assert_eq!(rows, count * 3);
                            assert!(stats.first_painted.is_some_and(|first| first > rows - 30));
                            assert!(stats.laid_out <= 30, "{stats:?}");
                            assert!((1..=30).contains(&chat.transcript_parse_requests()));
                            assert!(window.shot_element_bounds("chat-input").is_some());
                            stats
                        })?;
                        runner.update(cx, |view, _, cx| view.select_thread("thread-empty".into(), cx))?;
                        runner.frame(cx)?;
                        let start = Instant::now();
                        runner.update(cx, |view, _, cx| view.select_thread(session.into(), cx))?;
                        runner.frame(cx)?;
                        eprintln!(
                            "{} events: replay to first chat and composer frame={first_frame_ms} ms, \
                             cached switch to first frame={} ms, stats={stats:?}",
                            count * 5, start.elapsed().as_millis(),
                        );
                    }
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn transcript_parse_work_depends_on_the_viewport_not_history_length() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1200., 760.).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    let mut counts = Vec::new();
                    for (session, count) in [("short", 20), ("long", 500)] {
                        runner.update(cx, |view, _, cx| {
                            for index in 0..count {
                                view.on_event(ServerEvent::TextDelta {
                                    session: session.into(),
                                    item: format!("user:{index}"),
                                    actor: fuzukue_proto::Actor::User,
                                    text: "**Markdown reply** with `code`.".into(),
                                }, cx);
                            }
                            assert_eq!(view.chats[session].transcript_parse_requests(), 0);
                            view.select_thread(session.into(), cx);
                        })?;
                        runner.frame(cx)?;
                        counts.push(runner.update(cx, |view, _, _| {
                            let chat = &view.chats[session];
                            (chat.transcript_parse_requests(), chat.scroller.frame_stats().laid_out)
                        })?);
                    }
                    assert_eq!(counts[0], counts[1]);
                    assert!(counts[1].0 > 0 && counts[1].0 < 20, "{counts:?}");
                    runner.update(cx, |view, _, cx| view.select_thread("short".into(), cx))?;
                    runner.frame(cx)?;
                    assert_eq!(runner.update(cx, |view, _, _| view.chats["short"].transcript_parse_requests())?, counts[0].0);
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn table_colors_scenario_paints_the_resolved_theme() {
        use fuzukue_view::appearance::ThemePreset;
        use gpui_kit::component::ActiveTheme as _;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1360., 860.).unwrap();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let settings = tempfile::tempdir()?;
                    runner.update(cx, |view, _, _| {
                        view.appearance.isolate_shot_settings(settings.path().join("settings.toml"));
                    })?;
                    let scenario: Scenario = serde_json::from_str(include_str!("../../../tests/shots/table-colors.json"))?;
                    for step in scenario.steps {
                        let Step::Screenshot { name } = step else {
                            runner.step(step, cx).await?;
                            continue;
                        };
                        runner.frame(cx)?;
                        runner.update(cx, |view, window, cx| {
                            let (preset, dark) = match name.as_str() {
                                "table-classic-dark" => (ThemePreset::Classic, true),
                                "table-parchment-light" => (ThemePreset::Parchment, false),
                                _ => bail!("unexpected table capture {name}"),
                            };
                            let theme = cx.theme();
                            assert_eq!(view.appearance.settings.theme.preset, preset);
                            assert_eq!(theme.is_dark(), dark);
                            let bubble = window.shot_element_bounds("chat-message-bubble-0")
                                .context("missing reply")?.scale(window.scale_factor());
                            let quads = window.painted_quads();
                            let find_fill = |color| quads.iter().find(|quad| {
                                quad.bounds.intersects(&bubble)
                                    && quad.background.as_solid() == Some(color)
                                    && quads.iter().any(|border| {
                                        border.bounds == quad.bounds
                                            && border.border_color == theme.table_row_border
                                            && border.border_widths != Default::default()
                                    })
                            });
                            let frame = find_fill(theme.table).context("missing table frame and body fill")?;
                            let head = find_fill(theme.table_head).context("missing header fill and divider")?;
                            let stripe = find_fill(theme.table_even).context("missing alternating row and divider")?;
                            assert!(frame.bounds.intersects(&head.bounds) && frame.bounds.intersects(&stripe.bounds));
                            for (row, foreground) in [(head, theme.table_head_foreground), (stripe, theme.foreground)] {
                                let glyphs = window.painted_glyphs()
                                    .filter(|(bounds, _)| row.bounds.intersects(bounds))
                                    .map(|(_, color)| color)
                                    .collect::<Vec<_>>();
                                assert!(!glyphs.is_empty(), "missing table glyphs in {name}");
                                assert!(glyphs.iter().all(|color| *color == foreground), "unexpected glyph colors in {name}: {glyphs:?}");
                            }
                            Ok::<_, anyhow::Error>(())
                        })??;
                    }
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn theme_gallery_scenario_selects_every_preset_in_both_modes() {
        use fuzukue_view::appearance::ThemePreset;
        use gpui_kit::component::ActiveTheme as _;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1440., 960.).unwrap();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let settings = tempfile::tempdir()?;
                    runner.update(cx, |view, _, _| {
                        view.appearance.isolate_shot_settings(settings.path().join("settings.toml"));
                    })?;
                    let scenario: Scenario = serde_json::from_str(include_str!(
                        "../../../tests/shots/theme-presets-expanded.json"
                    ))?;
                    let mut captures = std::collections::HashSet::new();
                    for step in scenario.steps {
                        runner.frame(cx)?;
                        match step {
                            Step::Screenshot { name } => {
                                let actual = runner.update(cx, |view, _, cx| {
                                    let preset = serde_json::to_value(view.appearance.settings.theme.preset)
                                        .unwrap();
                                    let mode = if cx.theme().is_dark() { "dark" } else { "light" };
                                    format!("{}-{mode}", preset.as_str().unwrap())
                                })?;
                                assert_eq!(name, actual);
                                assert!(captures.insert(name));
                            }
                            // The headless test opens at the scenario's size.
                            Step::Size { .. } => {}
                            step => runner.step(step, cx).await?,
                        }
                    }
                    assert_eq!(captures.len(), ThemePreset::ALL.len() * 2);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn checked_in_scenarios_are_valid_json_steps() {
        for scenario in [
            include_str!("../../../tests/shots/chat.json"),
            include_str!("../../../tests/shots/chat-live.json"),
            include_str!("../../../tests/shots/table-colors.json"),
            include_str!("../../../tests/shots/thread-switch.json"),
            include_str!("../../../tests/shots/trash.json"),
            include_str!("../../../tests/shots/rewrite.json"),
            include_str!("../../../tests/shots/agents.json"),
            include_str!("../../../tests/shots/tiers.json"),
            include_str!("../../../tests/shots/skills.json"),
            include_str!("../../../tests/shots/planner.json"),
            include_str!("../../../tests/shots/composer.json"),
            include_str!("../../../tests/shots/composer-markdown.json"),
            include_str!("../../../tests/shots/composer-assist.json"),
            include_str!("../../../tests/shots/clipboard-attachment.json"),
            include_str!("../../../tests/shots/client-screenshot.json"),
            include_str!("../../../tests/shots/github-report.json"),
            include_str!("../../../tests/shots/deck-overview.json"),
            include_str!("../../../tests/shots/sidebar.json"),
            include_str!("../../../tests/shots/title-bar.json"),
            include_str!("../../../tests/shots/safe-close.json"),
            include_str!("../../../tests/shots/distribution.json"),
            include_str!("../../../tests/shots/self-update.json"),
            include_str!("../../../tests/shots/library.json"),
            include_str!("../../../tests/shots/exploration.json"),
            include_str!("../../../tests/shots/extensions.json"),
            include_str!("../../../tests/shots/memos.json"),
            include_str!("../../../tests/shots/review-reports.json"),
            include_str!("../../../tests/shots/review-findings.json"),
            include_str!("../../../tests/shots/ledger.json"),
            include_str!("../../../tests/shots/workspaces.json"),
            include_str!("../../../tests/shots/paper.json"),
            include_str!("../../../tests/shots/open-paper.json"),
            include_str!("../../../tests/shots/card-edit.json"),
            include_str!("../../../tests/shots/change-history.json"),
            include_str!("../../../tests/shots/branch-review.json"),
            include_str!("../../../tests/shots/literate-lean.json"),
            include_str!("../../../tests/shots/lean-statement-review.json"),
            include_str!("../../../tests/shots/issue-threads.json"),
            include_str!("../../../tests/shots/handoff.json"),
            include_str!("../../../tests/shots/editor.json"),
            include_str!("../../../tests/shots/lsp.json"),
            include_str!("../../../tests/shots/i18n-en.json"),
            include_str!("../../../tests/shots/math-select.json"),
            include_str!("../../../tests/shots/math-quote.json"),
            include_str!("../../../tests/shots/pdf-popup.json"),
            include_str!("../../../tests/shots/thread-menu.json"),
            include_str!("../../../tests/shots/codex-partner.json"),
            include_str!("../../../tests/shots/git.json"),
            include_str!("../../../tests/shots/roles.json"),
            include_str!("../../../tests/shots/routing.json"),
            include_str!("../../../tests/shots/review-agents.json"),
            include_str!("../../../tests/shots/consistency-review.json"),
            include_str!("../../../tests/shots/workshop.json"),
        ] {
            let scenario: Scenario = serde_json::from_str(scenario).unwrap();
            assert!(!scenario.steps.is_empty());
        }
    }

    #[test]
    fn safe_close_preserves_the_managed_server_until_the_last_window_and_renders_shutdown() {
        use fuzukue_proto::{
            DrainProgress, DrainReason, DrainStatus, SelfUpdateBlockingThread, ShutdownWork,
        };
        use gpui_kit::component::WindowExt as _;
        use crate::ConnectionStatus;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (first, launcher) = open_workbench(cx).unwrap();
            first.update(cx, |_, window, cx| launcher.update(cx, |view, cx| {
                view.on_event(ServerEvent::TurnActivity { session: "active".into(), running: true }, cx);
                assert!(view.window_can_close(window, cx));
                assert!(!window.has_active_dialog(cx));
            })).unwrap();
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let threads: Vec<_> = (1..=7).map(|index| SelfUpdateBlockingThread {
                        workspace: "workspace".into(), id: format!("thread-{index}"),
                        title: Some(format!("Running thread {index}")),
                        running: true, worker: false, queued: false, paused: false, checking: false,
                    }).collect();
                    let idle = ShutdownWork { jobs_running: 0, threads: vec![], deadline_secs: 60 };
                    let running = ShutdownWork { jobs_running: 8, threads: threads.clone(), deadline_secs: 60 };
                    runner.wait_for(cx, "fixture connection status", TIMEOUT_MS,
                        |view, _| matches!(view.status, ConnectionStatus::Detail(_))).await?;
                    runner.update(cx, |view, _, _| {
                        view.out = out.into();
                        view.out.select_workspace(Some("workspace".into()));
                        view.protocol = fuzukue_proto::SAFE_CLOSE_PROTOCOL;
                        view.status = ConnectionStatus::Connected;
                        view.shutdown_work(running.clone());
                    })?;
                    // The launcher's window can close while it still owns the server.
                    let http = first.update(cx, |_, window, cx| launcher.update(cx, |view, cx| {
                        view.shutdown_work(running.clone());
                        view.connection.launch_owner_fixture(&mut view.http)?;
                        view.close_workbench(window, cx);
                        Ok::<_, anyhow::Error>(view.http.clone())
                    }))??;
                    runner.update(cx, |view, window, cx| {
                        assert_eq!(cx.windows().len(), 1);
                        assert!(!window.has_active_dialog(cx));
                        view.http = http;
                        assert!(view.connection.manages_server(&view.http, cx));
                        assert!(!view.window_can_close(window, cx));
                        assert!(!view.window_can_close(window, cx));
                    })?;

                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };
                    runner.step(wait("safe-close-more"), cx).await?;
                    runner.step(click("safe-close-cancel"), cx).await?;
                    assert!(messages.try_recv().is_err());
                    runner.update(cx, |view, window, cx| {
                        view.shutdown_work(idle.clone());
                        cx.global_mut::<crate::appearance::SaveServerOnClose>().0 = true;
                        assert!(!view.window_can_close(window, cx));
                        assert!(!window.has_active_dialog(cx));
                    })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::RequestDrain { reason: DrainReason::Shutdown }));

                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
                    let mut status = DrainStatus {
                        started_at: now - 12_000, reason: DrainReason::Shutdown, progress: DrainProgress::Draining,
                        jobs_running: 3, can_cancel: true, message: None,
                        blocking: fuzukue_proto::SelfUpdateWaitReason { threads, ..Default::default() },
                    };
                    runner.update(cx, |view, window, cx| view.drain_progress(status.clone(), window, cx))?;
                    runner.step(wait("safe-close-progress"), cx).await?;
                    runner.key(cx, "enter")?;
                    runner.step(wait("safe-close-progress"), cx).await?;
                    runner.step(click("safe-close-cancel"), cx).await?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::CancelDrain { started_at } if started_at == status.started_at));
                    status.progress = DrainProgress::Cancelled;
                    runner.update(cx, |view, window, cx| {
                        view.drain_progress(status.clone(), window, cx);
                        view.shutdown_work(running.clone());
                    })?;
                    runner.click_menu(cx, "menu-file", "menu-stop-server")?;
                    runner.step(wait("safe-close-now"), cx).await?;
                    runner.step(click("safe-close-now"), cx).await?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ForceShutdown { started_at: None }));
                    runner.update(cx, |view, window, cx| view.safe_close_error("Fixture rejection".into(), window, cx))?;
                    runner.step(wait("safe-close-save"), cx).await?;
                    runner.step(click("safe-close-save"), cx).await?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::RequestDrain { reason: DrainReason::Shutdown }));

                    status.started_at = now;
                    status.progress = DrainProgress::Draining;
                    runner.update(cx, |view, window, cx| view.drain_progress(status.clone(), window, cx))?;
                    runner.step(wait("safe-close-force"), cx).await?;
                    runner.step(click("safe-close-force"), cx).await?;
                    assert!(messages.try_recv().is_err());
                    runner.step(wait("safe-close-force-confirm"), cx).await?;
                    runner.step(click("safe-close-force-confirm"), cx).await?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ForceShutdown { started_at: Some(id) } if id == now));
                    status.progress = DrainProgress::Ready;
                    runner.update(cx, |view, window, cx| {
                        view.connection.stop_launched_server(&view.http, cx)?;
                        view.drain_progress(status, window, cx);
                        Ok::<_, anyhow::Error>(())
                    })??;
                    assert!(cx.update(|cx| cx.windows().is_empty()));
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn thread_titles_and_workspace_footer_leave_room_for_descenders() {
        use crate::appearance::UI_LINE_HEIGHT;
        use gpui_kit::component::Theme;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                    id: "descenders".into(),
                    title: "dogfoodingやっていこう · gjpqy".into(),
                    updated: 0,
                    untitled: false,
                    archived: false,
                    pinned: false,
                    attached: vec![],
                    auto_attached: vec![],
                    suggestions: vec![],
                    math_macros: vec![],
                    artifacts: vec![],
                    github: Default::default(),
                    claude_session: None,
                    partner_settings: None,
                    worker_settings: None,
                    agent_overrides: Default::default(),
                }] }, cx);
                view.select_thread("descenders".into(), cx);
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = (|| {
                    for font_size in [16., *fuzukue_view::appearance::UI_FONT_SIZE.end()] {
                        runner.update(cx, |_, window, cx| {
                            Theme::update(cx, |theme| theme.font_size = px(font_size));
                            window.refresh();
                        })?;
                        runner.frame(cx)?;
                        runner.update(cx, |_, window, _| -> Result<()> {
                            let rem = px(font_size);
                            let line_height = |scale: f32| window.pixel_snap(
                                UI_LINE_HEIGHT.to_pixels((rem * scale).into(), rem),
                            );
                            for id in ["thread-row-title-descenders", "thread-header-title", "workspace-account"] {
                                let bounds = window.shot_element_bounds(id)
                                    .with_context(|| format!("element is not visible: {id}"))?;
                                let required = line_height(0.875) + if id == "workspace-account" {
                                    line_height(0.75)
                                } else {
                                    px(0.)
                                };
                                ensure!(bounds.size.height >= required,
                                    "{id} at {font_size}px needs {required}, got {}", bounds.size.height);
                            }
                            Ok(())
                        })??;
                    }
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn exploration_expands_groups_and_opens_records() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.out = out.into();
                view.deck = "research/curvature".into();
                view.exploration = crate::exploration::Pane::with_nodes("Curvature".into(),
                    serde_json::from_value(serde_json::json!([
                        {"id":"topic","title":"Curvature","depth":0,"kind":"category","state":"active"},
                        {"id":"question","title":"Gaussian bound","depth":1,"kind":"question","state":"queued",
                            "record":{"kind":"card","path":"papers/bounds/cards/gaussian.md","deck":"papers/bounds","card":"gaussian"}},
                        {"id":"note","title":"Notes","depth":1,"kind":"task","state":"done",
                            "record":{"kind":"file","path":"research/curvature/claims.yaml"}}
                    ])).unwrap());
                view.right = RightPane::Exploration;
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let visible = |runner: &Runner, cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "exploration-record-question")?);
                    runner.step(click("exploration-branch-topic"), cx).await?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "exploration-record-question")?);
                    runner.step(click("exploration-mode-state"), cx).await?;
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "exploration-record-question")?);
                    assert!(!visible(&runner, cx, "exploration-branch-topic")?);
                    runner.step(click("exploration-record-question"), cx).await?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelectPaper { deck } if deck == "papers/bounds"));
                    runner.update(cx, |view, _, cx| {
                        assert_eq!(view.pending_paper_open.as_ref().and_then(|(_, card, _)| card.as_deref()), Some("gaussian"));
                        for event in [
                            serde_json::json!({"type":"workspace","deck":"papers/bounds","tabs":[]}),
                            serde_json::json!({"type":"cards","deck":"papers/bounds","diagnostics":[],"cards":[{
                                "id":"gaussian","title":"Gaussian bound","kind":"theorem","status":"done",
                                "path":"papers/bounds/cards/gaussian.md","body":"A bound.",
                                "links":[],"backlinks":[],"files":[],"external":false
                            }]}),
                            serde_json::json!({"type":"document","title":"Bounds","sections":[],"macros":[]}),
                        ] {
                            view.on_event(serde_json::from_value(event).unwrap(), cx);
                        }
                        assert_eq!(view.right, RightPane::Card);
                        assert_eq!(view.selected.as_deref(), Some("gaussian"));
                    })?;
                    runner.step(Step::WaitElement { id: "edit-card".into() }, cx).await?;
                    runner.update(cx, |view, _, cx| {
                        view.right = RightPane::Exploration;
                        cx.notify();
                    })?;
                    runner.step(click("exploration-record-note"), cx).await?;
                    assert!(runner.update(cx, |view, _, _| {
                        view.right == RightPane::Note &&
                            view.note.as_ref().is_some_and(|(path, _)| path == "research/curvature/claims.yaml")
                    })?);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn research_issues_open_beside_chat_with_body_history_and_actions() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.out = out.into();
                view.on_event(serde_json::from_value(serde_json::json!({
                    "type":"research_issues", "repo":"owner/labo", "error":null, "issues":[{
                        "number":12, "title":"確認する", "url":"https://github.com/example-owner/example-project/issues/12",
                        "kind":"lean", "acceptance":"条件", "cards":["labo:harness#card"],
                        "card_workspaces":[null], "state":"ready", "assignee":"fixture",
                        "workspace":null, "session":null, "summary":null,
                        "details":{"body":"**本文**","created_at":"2026-01-01","closed_at":null,
                            "comments":[{"body":"続報","author":"fixture","created_at":null}]}
                    }]
                })).unwrap(), cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };
                    runner.click_menu(cx, "menu-view", "tool-research-issues")?;
                    runner.step(wait("research-issue-list"), cx).await?;
                    assert!(runner.update(cx, |view, window, _| {
                        view.right == crate::RightPane::ResearchIssues
                            && window.shot_element_bounds("chat-input").is_some()
                            && window.shot_element_bounds("right-pane-splitter").is_some()
                    })?);
                    runner.step(wait("research-issue-12-start"), cx).await?;
                    runner.step(click("research-issue-12"), cx).await?;
                    runner.step(wait("research-issue-body"), cx).await?;
                    runner.step(wait("research-issue-comment-0"), cx).await?;
                    runner.step(click("research-issues-back"), cx).await?;
                    runner.step(click("research-issue-12-start"), cx).await?;
                    assert!(messages.try_iter().any(|message| matches!(
                        message, ClientMsg::StartResearchIssue { number: 12 }
                    )));
                    runner.step(Step::Key { key: "escape".into() }, cx).await?;
                    runner.step(wait("chat-input"), cx).await?;
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn worker_approval_picker_uses_the_saved_workspace_snapshot() {
        use fuzukue_proto::WorkerApprovalPolicy::{Advisor, Ask};
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.out = out.into();
                view.switch_session("worker-approval".into(), cx);
                view.on_event(ServerEvent::ChatCapabilities {
                    effort_levels: Vec::new(), mail_restricted: false, mcp_tools: Vec::new(), folder_attachments: false,
                }, cx);
                view.on_event(ServerEvent::WorkerApprovalPolicyChanged { policy: Ask, shell_rules: None }, cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let steps: Scenario = serde_json::from_str(r#"{"steps":[
                        {"action":"wait_element","id":"permission-picker"},
                        {"action":"click","id":"worker-approval-picker"},
                        {"action":"key","key":"down"},
                        {"action":"key","key":"down"},
                        {"action":"key","key":"enter"}
                    ]}"#)?;
                    for step in steps.steps {
                        runner.step(step, cx).await?;
                    }
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::SetWorkerApprovalPolicy { policy: Advisor })));
                    runner.update(cx, |view, window, cx| {
                        assert_eq!(view.worker_approval_policy, Some(Ask));
                        view.on_event(ServerEvent::WorkerApprovalPolicyChanged { policy: Advisor, shell_rules: None }, cx);
                        assert_eq!(view.worker_approval_policy, Some(Advisor));
                        view.clear_workspace(window, cx);
                        assert_eq!(view.worker_approval_policy, None);
                        view.switch_session("worker-approval".into(), cx);
                        view.on_event(ServerEvent::WorkerApprovalPolicyChanged { policy: Advisor, shell_rules: None }, cx);
                    })?;
                    runner.step(Step::WaitElement { id: "worker-approval-picker".into() }, cx).await?;
                    assert!(runner.update(cx, |view, _, _| view.worker_approval_policy == Some(Advisor))?);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn approval_decisions_fold_and_pending_buttons_stay_full_size() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.out = out.into();
                view.switch_session("approvals".into(), cx);
                for id in ["approved", "denied", "pending"] {
                    view.on_event(ServerEvent::ApprovalRequested {
                        session: "approvals".into(), request_id: id.into(), actor: fuzukue_proto::Actor::Claude,
                        summary: long_approval_command(), localized: None, shell_directory: None, shell_prefix: None,
                    }, cx);
                }
                for (id, decision) in [("approved", "accept"), ("denied", "decline")] {
                    view.on_event(ServerEvent::ApprovalResolved {
                        request_id: id.into(), decision: decision.into(), automatic: false,
                    }, cx);
                }
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };
                    runner.step(wait("decided-approvals-approved"), cx).await?;
                    runner.step(wait("accept-pending"), cx).await?;
                    runner.step(wait("decline-pending"), cx).await?;
                    runner.step(wait("approval-details-pending"), cx).await?;
                    // The pending card is pinned in the dock above the composer, and
                    // the transcript keeps a one-line marker at the request's place.
                    assert!(runner.update(cx, |_, window, _| {
                        let dock = window.shot_element_bounds("approval-dock").unwrap();
                        let allow = window.shot_element_bounds("accept-pending").unwrap();
                        let composer = window.shot_element_bounds("chat-input").unwrap();
                        let marker = window.shot_element_bounds("approval-in-dock-pending").unwrap();
                        allow.top() >= dock.top() && allow.bottom() <= dock.bottom()
                            && dock.bottom() <= composer.top() && marker.bottom() <= dock.top()
                    })?);
                    let collapsed = runner.update(cx, |_, window, _| {
                        (window.shot_element_bounds("accept-pending").unwrap(),
                            window.shot_element_bounds("approval-preview-pending").unwrap())
                    })?;
                    runner.step(click("approval-details-pending"), cx).await?;
                    runner.wait_for(cx, "expanded approval and stable buttons", TIMEOUT_MS, |_, window| {
                        let Some(expanded) = window.shot_element_bounds("approval-preview-pending") else { return false };
                        let Some(allow) = window.shot_element_bounds("accept-pending") else { return false };
                        expanded.size.height > collapsed.1.size.height
                            && allow.origin.y - expanded.origin.y == collapsed.0.origin.y - collapsed.1.origin.y
                    }).await?;
                    runner.step(click("approval-details-pending"), cx).await?;
                    assert!(runner.update(cx, |_, window, _| {
                        let allow = window.shot_element_bounds("accept-pending").unwrap().size.height;
                        let deny = window.shot_element_bounds("decline-pending").unwrap().size.height;
                        let disclosure = window.shot_element_bounds("decided-approvals-approved").unwrap().size.height;
                        allow == deny && disclosure >= px(28.)
                            && window.shot_element_bounds("decided-approval-approved").is_none()
                            && window.shot_element_bounds("decided-approval-denied").is_none()
                    })?);
                    runner.step(click("decided-approvals-approved"), cx).await?;
                    runner.step(wait("decided-approval-approved"), cx).await?;
                    runner.step(wait("decided-approval-denied"), cx).await?;
                    runner.step(click("decline-pending"), cx).await?;
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::Resolve { request_id, decision } if request_id == "pending" && decision == "decline"
                    )));
                    runner.update(cx, |view, _, cx| {
                        let events = [
                            ServerEvent::ApprovalResolved {
                                request_id: "pending".into(), decision: "decline".into(), automatic: false,
                            },
                            ServerEvent::ToolCall {
                                session: "approvals".into(), item: "delegate:fixture".into(),
                                actor: fuzukue_proto::Actor::System, name: "delegate".into(),
                                input: serde_json::json!({"title": "Git check", "model": "claude"}),
                            },
                            ServerEvent::ToolCall {
                                session: "approvals".into(), item: "delegate:fixture:tool".into(),
                                actor: fuzukue_proto::Actor::Claude, name: "Bash".into(),
                                input: serde_json::json!({"command": "git status"}),
                            },
                            ServerEvent::ApprovalRequested {
                                session: "approvals".into(), request_id: "delegate:fixture:approval".into(),
                                actor: fuzukue_proto::Actor::Claude, summary: "Bash: git status".into(),
                                localized: None, shell_directory: None, shell_prefix: None,
                            },
                        ];
                        for event in events {
                            view.on_event(event, cx);
                        }
                    })?;
                    runner.step(wait("delegate-tools-delegate:fixture"), cx).await?;
                    runner.step(wait("accept-delegate:fixture:approval"), cx).await?;
                    runner.step(wait("decline-delegate:fixture:approval"), cx).await?;
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::ApprovalResolved {
                            request_id: "delegate:fixture:approval".into(), decision: "cancelled".into(), automatic: false,
                        }, cx);
                    })?;
                    runner.step(wait("decided-approvals-delegate:fixture:approval"), cx).await?;
                    assert!(runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("accept-delegate:fixture:approval").is_none()
                            && window.shot_element_bounds("decline-delegate:fixture:approval").is_none()
                    })?);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn non_running_reviews_can_be_dismissed_and_empty_results_expire() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let status = |kind: &str, state: &str| ServerEvent::ReviewStatus {
                deck: "writing/draft/harness".into(), kind: kind.into(), state: state.into(),
                message: state.into(), model: None, started_at: (state == "running").then_some(1),
                count: (state == "done").then_some(usize::from(kind == "style")),
                card: (state == "done" && kind == "style").then(|| "intro".into()), details: None,
            };
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.on_event(status("style", "failed"), cx);
                view.on_event(status("math", "failed"), cx);
                view.on_event(status("lean", "failed"), cx);
                view.on_event(status("consistency", "waiting"), cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let dismiss = |kind| format!("dismiss-review-{kind}-writing/draft/harness");
                    for kind in ["style", "consistency"] {
                        runner.step(Step::Click { id: Some(dismiss(kind)), x: None, y: None }, cx).await?;
                    }
                    runner.update(cx, |view, _, cx| {
                        for kind in ["style", "consistency"] {
                            assert!(!view.review_jobs.contains_key(&("writing/draft/harness".into(), kind.into())));
                        }
                        view.on_event(status("math", "running"), cx);
                        view.on_event(status("lean", "done"), cx);
                    })?;
                    runner.step(Step::WaitElement { id: "stop-math-writing/draft/harness".into() }, cx).await?;
                    runner.step(Step::WaitElement { id: dismiss("lean") }, cx).await?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds(&dismiss("math")).is_none())?);
                    runner.update(cx, |view, _, cx| {
                        let mut stale = status("style", "failed");
                        if let ServerEvent::ReviewStatus { deck, .. } = &mut stale {
                            *deck = "removed".into();
                        }
                        view.on_event(stale, cx);
                        view.on_event(status("style", "done"), cx);
                        view.on_event(ServerEvent::ReviewDecksPruned { decks: vec!["removed".into()] }, cx);
                        assert!(!view.review_jobs.contains_key(&("removed".into(), "style".into())));
                        assert!(view.review_jobs.contains_key(&("writing/draft/harness".into(), "style".into())));
                    })?;
                    runner.wait_for(cx, "empty review to expire", 10_000, |view, _| {
                        !view.review_jobs.contains_key(&("writing/draft/harness".into(), "lean".into()))
                    }).await?;
                    runner.step(Step::WaitElement { id: "review-comments-style-writing/draft/harness".into() }, cx).await?;
                    runner.step(Step::Click { id: Some(dismiss("style")), x: None, y: None }, cx).await?;
                    assert!(runner.update(cx, |view, _, _| {
                        !view.review_jobs.contains_key(&("writing/draft/harness".into(), "style".into()))
                    })?);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn self_update_canary_settings_render_and_send_actions() {
        use fuzukue_proto::{SelfUpdateCanaryAction, SELF_UPDATE_CANARY_PROTOCOL};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1280., 1250.).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: true, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    runner.step(Step::SelfUpdatePreview { canary: true, waiting: false }, cx).await?;
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.status = crate::ConnectionStatus::Connected;
                        view.settings_open = true;
                        view.sidebar.server_settings = true;
                        view.protocol = SELF_UPDATE_CANARY_PROTOCOL - 1;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("self-update-canary").is_some())?);
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("self-update-canary-promote").is_none())?);
                    runner.update(cx, |view, _, cx| {
                        view.protocol = SELF_UPDATE_CANARY_PROTOCOL;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-canary-promote".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelfUpdateCanary { action: SelfUpdateCanaryAction::Promote }));
                    runner.click(cx, ClickTarget::Element { id: "self-update-canary-discard".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelfUpdateCanary { action: SelfUpdateCanaryAction::Discard }));
                    runner.update(cx, |view, _, cx| {
                        let status = view.self_update.status.as_mut().unwrap();
                        status.canary = true;
                        status.canary_status = None;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("self-update-canary-notice").is_some())?);
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("self-update-start").is_none())?);
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn self_update_wait_fits_narrow_settings_and_confirms_force() {
        use fuzukue_proto::{SelfUpdateWaitAction, SELF_UPDATE_WAIT_PROTOCOL};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 1100., 1250.).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: true, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    runner.step(Step::Locale { locale: "ja".into() }, cx).await?;
                    runner.step(Step::SelfUpdatePreview { canary: false, waiting: true }, cx).await?;
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.status = crate::ConnectionStatus::Connected;
                        view.settings_open = true;
                        view.sidebar.server_settings = true;
                        view.protocol = SELF_UPDATE_WAIT_PROTOCOL;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-force".into() })?;
                    assert!(messages.try_recv().is_err());
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-force-back".into() })?;
                    assert!(messages.try_recv().is_err());
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-force".into() })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-force-confirm".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelfUpdateWait {
                        started_at: 1, action: SelfUpdateWaitAction::Force,
                    }));
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "self-update-cancel".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelfUpdateWait {
                        started_at: 1, action: SelfUpdateWaitAction::Cancel,
                    }));

                    runner.frame(cx)?;
                    runner.update(cx, |_, window, _| -> Result<()> {
                        let bounds = |id: &str| window.shot_element_bounds(id)
                            .with_context(|| format!("element is not visible: {id}"));
                        assert!(window.shot_element_bounds("self-update-wait-blocker-0").is_none());
                        for id in ["self-update-wait-thread-0", "self-update-wait-blocker-1", "self-update-wait-blocker-3"] {
                            let row = bounds(id)?;
                            let title = bounds(&format!("{id}-title"))?;
                            let status = bounds(&format!("{id}-status"))?;
                            assert!(title.bottom() <= status.top() && status.right() <= row.right());
                            assert!(title.size.height <= px(24.) && status.size.height <= px(24.), "{id}: {title:?}, {status:?}");
                        }
                        Ok(())
                    })??;
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn distribution_settings_show_host_options_and_update_notice() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    runner.step(Step::DistributionPreview, cx).await?;
                    runner.update(cx, |view, _, cx| {
                        view.settings_open = true;
                        view.sidebar.server_settings = true;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    for id in ["distribution-host", "distribution-keep", "distribution-auto", "distribution-url", "client-update", "distribution-send"] {
                        assert!(runner.update(cx, |_, window, _| window.shot_element_bounds(id))?.is_some(), "{id}");
                    }
                    runner.click(cx, ClickTarget::Element { id: "distribution-keep".into() })?;
                    assert!(runner.update(cx, |view, _, _| view.appearance.keep_running)?);
                    runner.frame(cx)?;
                    runner.key(cx, "space")?;
                    cx.update_window(runner.handle, |_, window, cx| {
                        let keystroke = Keystroke::parse("space").unwrap();
                        window.dispatch_event(PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }), cx);
                    })?;
                    assert!(!runner.update(cx, |view, _, _| view.appearance.keep_running)?);
                    runner.click(cx, ClickTarget::Element { id: "distribution-host".into() })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("distribution-keep"))?.is_none());
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn title_search_labels_deck_kinds_and_dismisses_on_outside_click() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, _, cx| {
                        view.tabs = [
                            ("slides", "slides/levy-talk", "Lévy 族と球面の集中"),
                            ("paper", "papers/sample", "原稿の題"),
                            ("book", "books/sample", "本の題"),
                            ("research", "research/sample", "研究の題"),
                            ("document", "documents/sample", "資料の題"),
                        ].into_iter().map(|(kind, path, title)| fuzukue_proto::TabView {
                            id: path.into(), title: title.into(), kind: kind.into(), deck: Some(path.into()),
                            summary: String::new(), status: String::new(), next: String::new(),
                            plan: None, has_exploration: false, papers: vec![], notes: vec![], lean: vec![], related: vec![],
                        }).collect();
                        for (query, label) in [
                            ("levy-talk", "スライド · Lévy 族と球面の集中"),
                            ("原稿の題", "論文 · 原稿の題"), ("本の題", "本 · 本の題"),
                            ("研究の題", "研究 · 研究の題"), ("資料の題", "文書 · 資料の題"),
                        ] {
                            assert_eq!(view.palette_matches(query)[0].0, label);
                        }
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "tool-search".into() })?;
                    runner.frame(cx)?;
                    let bounds = runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("title-search")
                    })?.context("title search is not visible")?;
                    runner.click(cx, ClickTarget::Coordinates {
                        x: bounds.center().x.into(), y: (bounds.origin.y + px(4.)).into(),
                    })?;
                    assert!(runner.update(cx, |view, _, _| view.palette.is_some())?);
                    runner.click(cx, ClickTarget::Coordinates { x: 10., y: 12. })?;
                    assert!(runner.update(cx, |view, _, _| view.palette.is_none())?);
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn hamburger_view_menu_opens_the_library_and_supports_keyboard_navigation() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::LIBRARY_PROTOCOL;
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    let visible = |cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    let click = |cx: &mut AsyncApp, id: &str| -> Result<()> {
                        runner.click_target(cx, ClickTarget::Element { id: id.into() })?;
                        runner.frame(cx)
                    };

                    runner.frame(cx)?;
                    assert!(!visible(cx, "menu-file")?);
                    assert!(visible(cx, crate::menu_bar::SETTINGS_BUTTON)?);
                    click(cx, crate::menu_bar::MENU_BUTTON)?;
                    for group in ["menu-file", "menu-edit", "menu-view", "menu-thread", "menu-help"] {
                        assert!(visible(cx, group)?);
                    }
                    runner.hover_element(cx, "menu-view")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "tool-library")?);
                    click(cx, "menu-view")?;
                    assert!(!visible(cx, "tool-library")?);
                    runner.hover_element(cx, "menu-file")?;
                    runner.frame(cx)?;
                    assert!(!visible(cx, "menu-close-workspace")?);
                    click(cx, "menu-file")?;
                    assert!(visible(cx, "menu-close-workspace")?);
                    runner.hover_element(cx, "menu-view")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "tool-library")?);
                    assert!(!visible(cx, "menu-close-workspace")?);
                    click(cx, "tool-library")?;
                    assert!(visible(cx, "library-close")?);
                    assert!(!visible(cx, "menu-view")?);

                    click(cx, "library-close")?;
                    runner.key(cx, "alt-f10")?;
                    runner.frame(cx)?;
                    runner.key(cx, "left")?;
                    runner.key(cx, "down")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "tool-settings")?);
                    runner.key(cx, "right")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "menu-close-workspace")?);
                    runner.key(cx, "right")?;
                    runner.frame(cx)?;
                    runner.key(cx, "right")?;
                    runner.frame(cx)?;
                    runner.key(cx, "up")?;
                    runner.key(cx, "down")?;
                    runner.key(cx, "enter")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "library-close")?);

                    cx.update_window(handle, |_, window, cx| {
                        for modifiers in [gpui_kit::Modifiers { alt: true, ..Default::default() }, Default::default()] {
                            window.dispatch_event(PlatformInput::ModifiersChanged(gpui_kit::ModifiersChangedEvent {
                                modifiers,
                                capslock: Default::default(),
                            }), cx);
                        }
                    })?;
                    runner.frame(cx)?;
                    click(cx, "menu-view")?;
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "menu-view")?);
                    assert!(!visible(cx, "tool-library")?);
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(!visible(cx, "menu-view")?);

                    click(cx, crate::menu_bar::MENU_BUTTON)?;
                    click(cx, "menu-view")?;
                    click(cx, "tool-search")?;
                    assert!(!visible(cx, "menu-view")?);
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;

                    click(cx, crate::menu_bar::MENU_BUTTON)?;
                    click(cx, "menu-help")?;
                    click(cx, crate::menu_bar::SETTINGS_BUTTON)?;
                    assert!(runner.update(cx, |view, _, _| view.settings_open && !view.sidebar.server_settings)?);
                    assert!(visible(cx, "close-settings")?);
                    assert!(!visible(cx, "menu-help")?);
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn library_search_uses_the_debounced_input_and_opens_a_result() {
        use fuzukue_proto::{Envelope, LibraryHit, LibraryKind, LibraryLocation, TabView, LIBRARY_DECKS_PROTOCOL, LIBRARY_PROTOCOL};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = LIBRARY_PROTOCOL - 1;
                view.out = out.into();
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    assert!(runner.update(cx, |view, _, _| view.right == RightPane::None)?);
                    assert!(messages.try_recv().is_err());
                    runner.key(cx, "escape")?;
                    runner.update(cx, |view, _, cx| {
                        view.protocol = LIBRARY_PROTOCOL;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    runner.frame(cx)?;
                    runner.type_text(cx, "球面")?;
                    assert!(messages.try_recv().is_err());
                    let started = Instant::now();
                    let request = loop {
                        if let Ok(message) = messages.try_recv() {
                            let ClientMsg::SearchLibrary { request, query, offset: 0, .. } = message else {
                                anyhow::bail!("expected the first library search");
                            };
                            assert_eq!(query, "球面");
                            break request;
                        }
                        ensure!(started.elapsed() < Duration::from_secs(3), "library query was never sent");
                        pause(cx, 20).await;
                    };
                    runner.click(cx, ClickTarget::Element { id: "library-kind-1".into() })?;
                    let ClientMsg::SearchLibrary { request: filtered, kinds, .. } = messages.try_recv()? else {
                        anyhow::bail!("expected the filtered library search");
                    };
                    assert_eq!(kinds, [LibraryKind::Card]);
                    let hit = LibraryHit {
                        title: "球面の集中".into(), deck: Some("paper".into()),
                        location: LibraryLocation::Card { card: "sphere".into() },
                        snippet: "球面の集中を述べる。".into(), highlights: vec![[0, 6]], score: 3., updated: 0,
                    };
                    let receive = |request| conn::ConnEvent::Event(Envelope { seq: 0, workspace: None,
                        event: ServerEvent::LibraryResults { request, hits: vec![hit.clone()], more: false, decks: vec!["paper".into()] },
                    });
                    runner.update(cx, |view, window, cx| view.on_conn(receive(request), window, cx))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("library-hit-0"))?.is_none());
                    runner.update(cx, |view, window, cx| view.on_conn(receive(filtered), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "library-hit-0".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelectPaper { deck } if deck == "paper"));

                    runner.update(cx, |view, _, cx| {
                        view.protocol = LIBRARY_DECKS_PROTOCOL;
                        view.tabs.push(TabView {
                            id: "paper".into(), title: "球面の集中".into(), kind: "paper".into(), deck: Some("paper".into()),
                            summary: String::new(), status: String::new(), next: String::new(),
                            plan: None, has_exploration: false, papers: vec![], notes: vec![], lean: vec![], related: vec![],
                        });
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SearchLibrary { .. }));
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "library-kind-1".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SearchLibrary { kinds, .. } if kinds.is_empty()));
                    runner.click(cx, ClickTarget::Element { id: "library-kind-6".into() })?;
                    let ClientMsg::SearchLibrary { request, kinds, .. } = messages.try_recv()? else {
                        anyhow::bail!("expected the deck search");
                    };
                    assert_eq!(kinds, [LibraryKind::Deck]);
                    let hit = LibraryHit { location: LibraryLocation::Deck, ..hit };
                    runner.update(cx, |view, window, cx| view.on_conn(conn::ConnEvent::Event(Envelope {
                        seq: 0, workspace: None,
                        event: ServerEvent::LibraryResults { request, hits: vec![hit], more: false, decks: vec!["paper".into()] },
                    }), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "library-hit-0".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelectDeck { deck } if deck == "paper"));
                    assert!(runner.update(cx, |view, _, _| view.right == RightPane::Read)?);
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn extension_permissions_and_view_use_the_protocol() {
        use fuzukue_proto::{Envelope, ExtensionInfo, ExtensionNode, ExtensionPermissions, EXTENSIONS_PROTOCOL};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.set_reduce_motion(true);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = EXTENSIONS_PROTOCOL;
                view.out = out.into();
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let item = ExtensionInfo {
                        id: "sample-id".into(), name: "memo-stats".into(), title: "Memo statistics".into(),
                        description: String::new(), source: "memo-stats".into(), version: "1".into(),
                        permissions: ExtensionPermissions { read: vec!["memos/*/card.md".into()], agent_tools: true, ..Default::default() },
                        enabled: false, view: true, approval: Some("fingerprint".into()), error: None,
                    };
                    let event = |event| conn::ConnEvent::Event(Envelope { seq: 0, workspace: None, event });
                    runner.update(cx, |view, window, cx| {
                        view.on_conn(event(ServerEvent::Extensions { items: vec![item.clone()] }), window, cx);
                        view.on_conn(event(ServerEvent::ExtensionPermissionPrompt { item: item.clone() }), window, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("extension-view-memo-stats"))?.is_none());
                    runner.click(cx, ClickTarget::Element { id: "extension-approve".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::GrantExtension { id, approval }
                        if id == "sample-id" && approval == "fingerprint"));

                    let mut item = item;
                    item.enabled = true;
                    item.approval = None;
                    runner.update(cx, |view, window, cx| {
                        view.on_conn(event(ServerEvent::Extensions { items: vec![item.clone()] }), window, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "extension-view-memo-stats")?;
                    let ClientMsg::RenderExtension { id, request } = messages.try_recv()? else {
                        anyhow::bail!("expected extension render request");
                    };
                    let nodes = vec![
                        ExtensionNode::Heading { text: "Memo statistics".into() },
                        ExtensionNode::Table { columns: vec!["Metric".into(), "Count".into()], rows: vec![vec!["Memos".into(), "1".into()]] },
                        ExtensionNode::Input { id: "query".into(), label: "Query".into(), value: String::new() },
                        ExtensionNode::Button { id: "refresh".into(), text: "Refresh".into() },
                    ];
                    let view = |request| event(ServerEvent::ExtensionView { id: id.clone(), request, result: Ok(nodes.clone()) });
                    runner.update(cx, |this, window, cx| this.on_conn(view(request + 1), window, cx))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("extension-table-extension-1"))?.is_none());
                    runner.update(cx, |this, window, cx| this.on_conn(view(request), window, cx))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("extension-table-extension-1"))?.is_some());
                    runner.click(cx, ClickTarget::Element { id: "extension-input-query".into() })?;
                    runner.frame(cx)?;
                    runner.type_text(cx, "memos")?;
                    runner.key(cx, "enter")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ExtensionUiEvent { id, element, value: Some(value), .. }
                        if id == "sample-id" && element == "query" && value == "memos"));
                    runner.click(cx, ClickTarget::Element { id: "extension-button-refresh".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ExtensionUiEvent { id, element, value: None, .. }
                        if id == "sample-id" && element == "refresh"));
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn memos_use_the_library_composer_editor_and_deck_picker() {
        use fuzukue_proto::{DeckRoute, Envelope, LibraryHit, LibraryLocation, SourceSaveOutcome, MEMOS_PROTOCOL};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = MEMOS_PROTOCOL;
                view.out = out.into();
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let click = |id: &str| ClickTarget::Element { id: id.into() };
                    let event = |event| conn::ConnEvent::Event(Envelope { seq: 0, workspace: None, event });
                    let body = "球面の集中\n次に調べること";
                    let id = "2026-11-20-deadbeef";
                    let path = format!("memos/{id}/card.md");
                    let raw = format!("---\nid: {id}\nkind: memo\ntitle: 球面の集中\n---\n{body}");
                    let hit = LibraryHit {
                        title: "球面の集中".into(), deck: Some("memos".into()),
                        location: LibraryLocation::Memo { card: id.into(), path: path.clone() },
                        snippet: body.into(), highlights: vec![], score: 0., updated: 0,
                    };
                    let results = |request, hits| event(ServerEvent::LibraryResults {
                        request, hits, more: false, decks: vec!["memos".into(), "paper".into()],
                    });
                    let recent = |message| match message {
                        ClientMsg::SearchLibrary { request, query, offset: 0, .. } if query.is_empty() => Ok(request),
                        _ => anyhow::bail!("expected a recent memo search"),
                    };

                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    let initial = recent(messages.try_recv()?)?;
                    runner.update(cx, |view, window, cx| view.on_conn(results(initial, vec![]), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, click("library-memo-input"))?;
                    runner.type_text(cx, body)?;
                    runner.key(cx, "Ctrl+Enter")?;
                    let ClientMsg::AddMemo { request, text, from: None } = messages.try_recv()? else {
                        anyhow::bail!("expected to save the memo");
                    };
                    assert_eq!(text, body);
                    runner.update(cx, |view, window, cx| view.on_conn(event(ServerEvent::MemoResult { request, error: None }), window, cx))?;
                    let refreshed = recent(messages.try_recv()?)?;
                    runner.update(cx, |view, window, cx| view.on_conn(results(refreshed, vec![hit.clone()]), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, click("library-hit-0"))?;
                    runner.update(cx, |view, window, cx| {
                        let tab = view.source.active_id().expect("opened memo");
                        view.complete_source_load_for_shot(tab, &raw, window, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.key(cx, "Ctrl+End")?;
                    runner.type_text(cx, "（追記）")?;
                    runner.frame(cx)?;
                    runner.click(cx, click("source-save"))?;
                    let ClientMsg::SaveSource { request, edit } = messages.try_recv()? else {
                        anyhow::bail!("expected source editor save");
                    };
                    assert_eq!(edit.path, path);
                    assert!(edit.text.ends_with("（追記）"));
                    runner.update(cx, |view, window, cx| view.on_conn(event(ServerEvent::SourceSaveResult {
                        request, outcome: SourceSaveOutcome::Saved { text: edit.text, range: None },
                    }), window, cx))?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::EditorClosed { path: closed } if closed == path));
                    let after_edit = recent(messages.try_recv()?)?;
                    runner.update(cx, |view, window, cx| view.on_conn(results(after_edit, vec![hit]), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, click("library-move-memo-0"))?;
                    runner.frame(cx)?;
                    runner.type_text(cx, "destination")?;
                    let started = Instant::now();
                    loop {
                        if let Ok(message) = messages.try_recv() {
                            assert!(matches!(message, ClientMsg::SearchDecks { query } if query == "destination"));
                            break;
                        }
                        ensure!(started.elapsed() < Duration::from_secs(3), "destination search was never sent");
                        pause(cx, 20).await;
                    }
                    runner.update(cx, |view, window, cx| view.on_conn(event(ServerEvent::DeckSearchResults {
                        query: "destination".into(), decks: vec![DeckRoute {
                            deck: "paper".into(), title: "Destination".into(), kind: "paper".into(),
                        }],
                    }), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, click("palette-0"))?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::MoveMemo { id: memo, deck, .. }
                        if memo == id && deck == "paper"));
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn chat_message_can_be_saved_as_a_memo_with_its_origin() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::MEMOS_PROTOCOL;
                view.active = "trial".into();
                view.out = out.into();
                view.on_chat_event(ServerEvent::UserPrompt {
                    session: "trial".into(), item: "user:42".into(), text: "試した結果を記録する".into(),
                    paths: vec![], to_worker: false,
                }, cx);
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "chat-save-memo-0".into() })?;
                    let ClientMsg::AddMemo { request, text, from: Some(from) } = messages.try_recv()? else {
                        anyhow::bail!("expected to save the chat message");
                    };
                    assert_eq!(text, "試した結果を記録する");
                    assert_eq!(from, "thread:trial#user:42");
                    runner.update(cx, |view, window, cx| view.on_conn(conn::ConnEvent::Event(fuzukue_proto::Envelope {
                        seq: 0, workspace: None, event: ServerEvent::MemoResult { request, error: None },
                    }), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "chat-save-memo-0".into() })?;
                    assert!(messages.try_recv().is_err());
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn peer_review_pane_shows_filtered_points_and_requires_confirmation() {
        use fuzukue_proto::{
            ReviewPoint, ReviewPointDraft, ReviewPointStatus, ReviewReport, ReviewReportSource,
            ReviewReportsAction as Action, ReviewSplit, ReviewSplitState, ReviewSubmission,
            SubmissionMetadata, REVIEW_REPORTS_PROTOCOL,
        };

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            let deck = "papers/demo";
            let id = "2026-11-20-jfa";
            view.update(cx, |view, cx| {
                view.protocol = REVIEW_REPORTS_PROTOCOL - 1;
                view.deck = deck.into();
                view.right = RightPane::Read;
                view.out = out.into();
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = (|| {
                    let visible = |cx: &mut AsyncApp, id| runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some());
                    let click = |cx: &mut AsyncApp, id: &str| {
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: id.into() })
                    };
                    runner.frame(cx)?;
                    assert!(!visible(cx, "paper-show-reviews")?);
                    runner.update(cx, |view, _, cx| {
                        view.protocol = REVIEW_REPORTS_PROTOCOL;
                        cx.notify();
                    })?;
                    click(cx, "paper-show-reviews")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ReviewReports { deck: path, action: Action::List, .. } if path == deck));

                    let mut entry = ReviewSubmission {
                        id: id.into(),
                        metadata: SubmissionMetadata { venue: "JFA".into(), revision: "v1".into(), submitted: "2026-11-20".into(), decision: String::new(), deadline: String::new() },
                        reports: vec![ReviewReport { number: 1, text: "Explain the bound.".into() }],
                        points: vec![ReviewPoint {
                            id: "R1.1".into(), report: 1, lines: [1, 1],
                            quote: "Explain the bound.".into(), summary: "Explain the bound".into(),
                            cards: vec!["bound".into()], status: ReviewPointStatus::Todo,
                            fixed_locations: Vec::new(), response: String::new(), thread: None,
                        }],
                        split_reports: vec![1], response: None,
                    };
                    let snapshot = |entry, splits, fixing| ServerEvent::ReviewReports { deck: deck.into(), revision: 0, submissions: vec![entry], splits, fixing };
                    runner.update(cx, |view, _, cx| view.on_event(snapshot(entry.clone(), vec![], vec![]), cx))?;
                    click(cx, "review-submission-2026-11-20-jfa")?;
                    click(cx, "review-filter-2")?;
                    runner.frame(cx)?;
                    assert!(!visible(cx, "review-point-R1.1")?);
                    click(cx, "review-filter-1")?;
                    click(cx, "review-point-R1.1")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "review-point-detail")?);
                    assert!(visible(cx, "review-card-bound")?);
                    click(cx, "review-fix")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ReviewReports { action: Action::Fix { submission, point }, .. } if submission == id && point == "R1.1"));
                    entry.points[0].thread = Some("fix-thread".into());
                    entry.points[0].status = ReviewPointStatus::InProgress;
                    runner.update(cx, |view, _, cx| view.on_event(snapshot(entry.clone(), vec![], vec!["fix-thread".into()]), cx))?;
                    click(cx, "review-set-status-2")?;
                    assert!(messages.try_recv().is_err());
                    runner.update(cx, |view, _, cx| view.on_event(snapshot(entry.clone(), vec![], vec![]), cx))?;
                    click(cx, "review-set-status-2")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ReviewReports { action: Action::Update { update, .. }, .. } if update.status == Some(ReviewPointStatus::Fixed)));

                    click(cx, "review-points")?;
                    click(cx, "review-add-paste")?;
                    click(cx, "review-field-0")?;
                    runner.type_text(cx, "Check the hypothesis.")?;
                    click(cx, "review-form-save")?;
                    let ClientMsg::ReviewReports { request, action: Action::Add { source: ReviewReportSource::Paste { text }, .. }, .. } = messages.try_recv()? else {
                        anyhow::bail!("expected pasted report");
                    };
                    assert_eq!(text, "Check the hypothesis.");
                    entry.reports.push(ReviewReport { number: 2, text });
                    let draft = ReviewPointDraft { lines: [1, 1], summary: "Check the hypothesis".into(), cards: vec!["bound".into()] };
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::ReviewReportsResult { deck: deck.into(), request, ok: true }, cx);
                        view.on_event(snapshot(entry, vec![ReviewSplit {
                            submission: id.into(), report: 2,
                            state: ReviewSplitState::Proposed { points: vec![draft.clone()] },
                        }], vec![]), cx);
                    })?;
                    click(cx, "review-report-2")?;
                    runner.frame(cx)?;
                    assert!(visible(cx, "review-proposal")?);
                    assert!(messages.try_recv().is_err());
                    click(cx, "review-confirm")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::ReviewReports { action: Action::Confirm { report: 2, points, .. }, .. } if points == vec![draft]));
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn ledger_filters_rows_shows_history_and_confirms_proposals() {
        use fuzukue_proto::{Envelope, LedgerAction, LedgerEntry, LedgerProposal, LedgerView, LEDGER_PROTOCOL};

        #[derive(Deserialize)]
        struct Fixture { entries: Vec<LedgerEntry> }

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.protocol = LEDGER_PROTOCOL - 1;
                view.out = out.into();
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(),
                    read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = (|| {
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    let click = |id: &str| ClickTarget::Element { id: id.into() };
                    // Opening the library also requests recent memos.
                    assert!(matches!(messages.try_recv()?, ClientMsg::SearchLibrary { query, kinds, .. }
                        if query.is_empty() && kinds.is_empty()));
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("library-ledger-tab"))?.is_none());
                    runner.update(cx, |view, _, cx| {
                        view.protocol = LEDGER_PROTOCOL;
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, click("library-ledger-tab"))?;
                    let ClientMsg::Ledger { request, action: LedgerAction::List } = messages.try_recv()? else {
                        anyhow::bail!("expected a ledger list");
                    };
                    let Fixture { entries } = toml::from_str(include_str!("../../../examples/lab/references/ledger.toml"))?;
                    let publisher = "publisher-supplement";
                    let proposal = LedgerProposal { id: 4, entry: entries[0].clone() };
                    let receive = |request, proposals| conn::ConnEvent::Event(Envelope {
                        seq: 0, workspace: None, event: ServerEvent::LedgerResult {
                            request, view: Some(LedgerView { entries: entries.clone(), proposals, downloads: Vec::new() }),
                        },
                    });
                    runner.update(cx, |view, window, cx| view.on_conn(receive(request, Vec::new()), window, cx))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("ledger-row-concentration-notes"))?.is_some());
                    runner.click(cx, click("ledger-filter-3"))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("ledger-row-concentration-notes"))?.is_none());
                    runner.click(cx, click(&format!("ledger-attempts-{publisher}")))?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("ledger-history-publisher-supplement"))?.is_some());
                    runner.click(cx, click(&format!("ledger-fetch-{publisher}")))?;
                    let ClientMsg::Ledger { request, action: LedgerAction::Fetch { key } } = messages.try_recv()? else {
                        anyhow::bail!("expected a ledger fetch");
                    };
                    assert_eq!(key, publisher);

                    runner.update(cx, |view, window, cx| view.on_conn(receive(request, vec![proposal]), window, cx))?;
                    runner.frame(cx)?;
                    runner.click(cx, click("ledger-accept-all"))?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::Ledger { action: LedgerAction::Accept { ids }, .. } if ids == [4]));
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn card_file_editor_shortcuts_preserve_the_revision_and_confirm_dirty_discard() {
        use fuzukue_proto::{Envelope, SourceSaveOutcome};
        use gpui_kit::EntityInputHandler as _;
        use sha2::{Digest, Sha256};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle, view, target: None, output: PathBuf::new(),
                    read_only: false, seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    let path = "cards/editor.md";
                    let base = format!(
                        "---\r\nid: editor\r\n---\r\n\r\n# Heading\r\n{}\r\n  1. item",
                        "A long line that wraps. ".repeat(12).trim_end(),
                    );
                    let open = |cx: &mut AsyncApp| runner.update(cx, |view, window, cx| {
                        view.install_card_file_editor(path.into(), base.clone(), window, cx);
                    });
                    runner.update(cx, |view, _, cx| {
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.out = out.into();
                        view.cards = vec![serde_json::from_value(serde_json::json!({
                            "id":"editor","kind":"note","title":"Editor","status":"","path":path,
                            "links":[],"backlinks":[],"files":[],"body":"Body","external":false
                        })).unwrap()];
                        view.select("editor".into(), cx);
                    })?;
                    open(cx)?;
                    runner.frame(cx)?;
                    runner.update(cx, |view, window, cx| {
                        let editor = &view.editing.as_ref().unwrap().input;
                        let map = editor.read(cx).display_map();
                        let wrapped = map.buffer_line_to_display_row_range(5).unwrap();
                        assert!(wrapped.len() > 1);
                        assert!(wrapped.clone().all(|row| map.display_row_to_buffer_line(row) == 5));
                        assert_eq!(map.display_row_to_buffer_line(wrapped.end), 6);
                        editor.update(cx, |input, cx| {
                            input.replace_and_mark_text_in_range(None, "(", Some(1..1), window, cx);
                        });
                    })?;
                    runner.frame(cx)?;
                    runner.key(cx, "Enter")?;
                    runner.key(cx, "Shift+Tab")?;
                    runner.key(cx, "Ctrl+S")?;
                    assert!(messages.try_recv().is_err());
                    runner.update(cx, |view, window, cx| {
                        view.editing.as_ref().unwrap().input.update(cx, |input, cx| {
                            assert!(input.marked_text_range(window, cx).is_some());
                            assert_eq!(input.text().to_string(), format!("({base}"));
                            input.unmark_text(window, cx);
                            input.set_value(&base, window, cx);
                        });
                    })?;
                    runner.frame(cx)?;
                    runner.key(cx, "Ctrl+F")?;
                    runner.frame(cx)?;
                    runner.type_text(cx, "item")?;
                    runner.key(cx, "Enter")?;
                    runner.key(cx, "Shift+Enter")?;
                    runner.frame(cx)?;
                    let search = |cx: &mut AsyncApp| runner.update(cx, |view, _, cx| {
                        view.editing.as_ref().map(|editor| editor.input.read(cx).search_session().open)
                    });
                    assert_eq!(search(cx)?, Some(true));
                    runner.key(cx, "Escape")?;
                    runner.frame(cx)?;
                    assert_eq!(search(cx)?, Some(false));
                    runner.key(cx, "Escape")?;
                    runner.frame(cx)?;
                    assert_eq!(search(cx)?, None);
                    assert!(messages.try_recv().is_err());

                    open(cx)?;
                    runner.frame(cx)?;
                    runner.key(cx, "Ctrl+End")?;
                    runner.key(cx, "Enter")?;
                    runner.frame(cx)?;
                    let buffer = |cx: &mut AsyncApp| runner.update(cx, |view, _, cx| {
                        view.editing.as_ref().unwrap().input.read(cx).text().to_string()
                    });
                    assert_eq!(buffer(cx)?, format!("{base}\r\n  2. "));
                    runner.key(cx, "Enter")?;
                    assert_eq!(buffer(cx)?, format!("{base}\r\n  "));

                    for (open, closer) in [("(", ")"), ("[", "]"), ("{", "}"), ("“", "”"), ("`", "`")] {
                        runner.type_text(cx, open)?;
                        assert!(buffer(cx)?.ends_with(&format!("{open}{closer}")));
                        let before = buffer(cx)?;
                        runner.type_text(cx, closer)?;
                        assert_eq!(buffer(cx)?, before);
                        runner.type_text(cx, " ")?;
                    }
                    runner.type_text(cx, "\"")?;
                    assert!(buffer(cx)?.ends_with(" \""));
                    runner.key(cx, "Home")?;
                    runner.key(cx, "Home")?;
                    runner.type_text(cx, "(")?;
                    assert!(buffer(cx)?.ends_with("()  () [] {} “” `` \""));
                    runner.key(cx, "Right")?;
                    runner.key(cx, "End")?;
                    runner.type_text(cx, "word")?;
                    runner.key(cx, "Left")?;
                    runner.type_text(cx, "[")?;
                    assert!(buffer(cx)?.ends_with("wor[d"));
                    runner.key(cx, "Ctrl+/")?;
                    assert!(buffer(cx)?.ends_with("wor[d -->"));
                    runner.key(cx, "Ctrl+/")?;
                    assert!(buffer(cx)?.ends_with("wor[d"));
                    runner.key(cx, "Ctrl+D")?;
                    let draft = buffer(cx)?;
                    assert_eq!(draft.lines().rev().take(2).collect::<Vec<_>>(), vec![draft.lines().last().unwrap(); 2]);

                    runner.key(cx, "Escape")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("card-discard"))?.is_some());
                    runner.key(cx, "Escape")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("card-discard"))?.is_none());

                    runner.key(cx, "Ctrl+S")?;
                    let ClientMsg::SaveSource { request, edit } = messages.try_recv()? else {
                        anyhow::bail!("Ctrl+S did not request a revision-aware card save");
                    };
                    assert_eq!((edit.path.as_str(), edit.range, edit.text.as_str()), (path, None, draft.as_str()));
                    assert_eq!(edit.base_hash, format!("{:x}", Sha256::digest(&base)));
                    let deliver = |cx: &mut AsyncApp, request, outcome| runner.update(cx, |view, window, cx| {
                        view.on_conn(conn::ConnEvent::Event(Envelope {
                            seq: 0, workspace: None, event: ServerEvent::SourceSaveResult { request, outcome }
                        }), window, cx);
                    });
                    let latest = format!("{base}\r\nDisk change");
                    deliver(cx, request, SourceSaveOutcome::Conflict { latest: latest.clone() })?;
                    runner.frame(cx)?;
                    let (banner, frame, status) = runner.update(cx, |_, window, _| (
                        window.shot_element_bounds("editor-conflict"),
                        window.shot_element_bounds("card-file-editor"),
                        window.shot_element_bounds("editor-status"),
                    ))?;
                    assert!(banner.unwrap().bottom() <= frame.unwrap().origin.y);
                    assert!(status.unwrap().origin.y >= frame.unwrap().bottom());
                    runner.click(cx, ClickTarget::Element { id: "card-editor-view-diff".into() })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("source-conflict-diff"))?.is_some());
                    runner.click(cx, ClickTarget::Element { id: "card-editor-overwrite".into() })?;
                    let ClientMsg::SaveSource { request, edit } = messages.try_recv()? else {
                        anyhow::bail!("overwrite did not retry against the latest revision");
                    };
                    assert_eq!(edit.base_hash, format!("{:x}", Sha256::digest(latest)));
                    assert_eq!(edit.text, draft);
                    deliver(cx, request, SourceSaveOutcome::Saved { text: draft, range: None })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::EditorClosed { path: closed } if closed == path));
                    assert_eq!(search(cx)?, None);
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn source_pane_loads_with_tabs_outline_and_a_full_height_editor() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            cx.spawn(async move |cx| {
                let runner = Runner {
                    handle,
                    view,
                    target: None,
                    output: PathBuf::new(),
                    read_only: false,
                    seen: HashMap::new(),
                };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, window, cx| {
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.open_source_file("experiment.rs".into(), None, window, cx);
                        let id = view.source.active_id().expect("opened source tab");
                        view.complete_source_load_for_shot(
                            id,
                            include_str!("../../../tests/shots/fixtures/editor/experiment.rs"),
                            window,
                            cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    let (root, pane, tabs, tab, outline, editor) =
                        runner.update(cx, |_, window, _| {
                            (
                                window.shot_element_bounds("workbench"),
                                window.shot_element_bounds("source-pane"),
                                window.shot_element_bounds("source-tabs"),
                                window.shot_element_bounds("source-tab-1"),
                                window.shot_element_bounds("source-outline"),
                                window.shot_element_bounds("source-editor"),
                            )
                        })?;
                    let tabs = tabs.ok_or_else(|| {
                        anyhow!("source tabs are not visible; root={root:?}, pane={pane:?}, tab={tab:?}, outline={outline:?}, editor={editor:?}")
                    })?;
                    let tab = tab.context("source tab is not visible")?;
                    let outline = outline.context("source outline is not visible")?;
                    let editor = editor.context("source editor is not visible")?;
                    ensure!(
                        tabs.origin.y < px(100.) && tab.origin.y < px(100.),
                        "source tabs start too low: {tabs:?}, {tab:?}"
                    );
                    ensure!(
                        outline.origin.y >= tabs.bottom(),
                        "outline is not below tabs: {outline:?} vs {tabs:?}"
                    );
                    ensure!(
                        editor.origin.y >= outline.bottom(),
                        "editor is not below outline: {editor:?} vs {outline:?}"
                    );
                    ensure!(
                        editor.size.height > px(300.),
                        "source editor is too short: {:?}",
                        editor.size.height
                    );
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn source_lsp_displays_diagnostics_and_hover_and_follows_cross_file_definition() {
        use fuzukue_proto::{LspBuffer, LspOutcome, LspQuery, LspStatus, LspUpdate};
        use serde_json::json;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = async {
                    runner.update(cx, |view, window, cx| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::LSP_PROTOCOL;
                        view.open_source_file("main.rs".into(), None, window, cx);
                        view.complete_source_load_for_shot(view.source.active_id().unwrap(),
                            include_str!("../../../tests/shots/fixtures/lsp/main.rs"), window, cx);
                    })?;
                    let ClientMsg::LspBuffer { document, update: LspBuffer::Open { version, .. } } = messages.try_recv()?
                        else { bail!("expected LSP open") };
                    runner.update(cx, |view, _, cx| {
                        view.on_lsp_document(document, LspUpdate::Status { status: LspStatus::Running }, cx);
                        view.on_lsp_document(document, LspUpdate::Diagnostics { version, diagnostics: vec![json!({
                            "range": {"start":{"line":2,"character":22}, "end":{"line":2,"character":34}},
                            "message": "Type mismatch", "severity":1
                        })] }, cx);
                    })?;
                    runner.step(Step::WaitSourceDiagnostics, cx).await?;
                    runner.step(Step::HoverSource { line: 2, col: 19 }, cx).await?;
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let request = loop {
                        if let Ok(ClientMsg::LspRequest { request, query: LspQuery::Hover { position }, .. }) = messages.try_recv() {
                            assert_eq!(position.line, 1);
                            break request;
                        }
                        pause(cx, 30).await;
                        ensure!(Instant::now() < deadline, "hover did not send an LSP request");
                    };
                    runner.update(cx, |view, _, _| view.on_lsp_result(request, LspOutcome::Value {
                        value: json!({"contents":{"kind":"markdown","value":"Returns a sample."}})
                    }))?;
                    runner.step(Step::WaitSourceHover, cx).await?;
                    runner.key(cx, "F12")?;
                    let ClientMsg::LspRequest { request, query: LspQuery::Definition { .. }, .. } = messages.try_recv()?
                        else { bail!("expected definition request") };
                    runner.update(cx, |view, _, _| view.on_lsp_result(request, LspOutcome::Value { value: json!({
                        "uri":"helper.rs", "range":{"start":{"line":0,"character":7},"end":{"line":0,"character":13}}
                    }) }))?;
                    runner.wait_for(cx, "definition tab", TIMEOUT_MS, |view, _| view.source.has_open_path("helper.rs")).await?;
                    runner.update(cx, |view, window, cx| {
                        view.complete_source_load_for_shot(view.source.active_id().unwrap(),
                            include_str!("../../../tests/shots/fixtures/lsp/helper.rs"), window, cx);
                        assert_eq!(view.source.active_editor().unwrap().read(cx).cursor_position().character, 7);
                    })?;
                    Ok::<_, anyhow::Error>(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn github_cards_disclose_markdown_and_send_one_bulk_request() {
        use fuzukue_proto::{GitHubActivity, GitHubActivityKind, GitHubAuthor, GitHubLink, IssueThreadAction, ThreadGitHub, ThreadView};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    let session = "github-fixture".to_owned();
                    let link = GitHubLink { repo: "fixture/repo".into(), number: 13 };
                    let activity = |id: &str, author, body: String| GitHubActivity {
                        id: id.into(), link: link.clone(), actor: "fixture".into(), author,
                        kind: GitHubActivityKind::Comment, excerpt: String::new(), body: Some(body),
                        url: "https://github.com/fixture/repo/issues/13".into(), at: String::new(),
                        review_comment: None,
                        intake: false,
                    };
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::ISSUE_RESPOND_PROTOCOL;
                        view.github.repo = link.repo.clone();
                        view.on_event(ServerEvent::GitHubActivity {
                            session: session.clone(), item: "activity".into(),
                            events: vec![
                                activity("owner", GitHubAuthor::Owner, "Try a different example.".into()),
                                activity("report", GitHubAuthor::Agent, format!(
                                    "### Findings\n\n[Example](https://example.com)\n\n### Verified\n\n{}", "A test passed.\n\n".repeat(3)
                                )),
                            ],
                        }, cx);
                        view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                            github: ThreadGitHub { links: vec![link.clone()], unread: 2, ..Default::default() },
                            untitled: false, id: session.clone(), title: "GitHub activity".into(), updated: 0,
                            archived: false, pinned: false, attached: vec![], auto_attached: vec![],
                            suggestions: vec![], math_macros: vec![], artifacts: vec![], claude_session: None,
                            partner_settings: None, worker_settings: None, agent_overrides: Default::default(),
                        }] }, cx);
                        view.select_thread(session.clone(), cx);
                    })?;
                    runner.frame(cx)?;
                    let has_element = |cx: &mut AsyncApp, id| runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some());
                    assert!(has_element(cx, "github-respond-owner")?);
                    assert!(!has_element(cx, "github-open-reply")?);
                    let report_height = |cx: &mut AsyncApp| runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("github-body-report").unwrap().size.height
                    });
                    let collapsed = report_height(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "github-full-report".into() })?;
                    runner.frame(cx)?;
                    let expanded = report_height(cx)?;
                    assert!(expanded > collapsed, "report height {collapsed:?} -> {expanded:?}");
                    runner.click(cx, ClickTarget::Element { id: "github-respond-all".into() })?;
                    let requests: Vec<_> = messages.try_iter().filter_map(|msg| match msg {
                        ClientMsg::IssueThreads { action: IssueThreadAction::Respond { session, mut events } } => {
                            events.sort();
                            Some((session, events))
                        }
                        _ => None,
                    }).collect();
                    assert_eq!(requests, [(session, vec!["owner".into(), "report".into()])]);
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn tests_panel_runs_stops_and_opens_a_failure_in_the_editor() {
        use fuzukue_proto::{TestFailure, TestRun, TestState, TestSuite};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, window, cx| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.open_source_file("analysis.py".into(), None, window, cx);
                        view.complete_source_load_for_shot(
                            view.source.active_id().unwrap(),
                            include_str!("../../../tests/shots/fixtures/tests/analysis.py"),
                            window, cx,
                        );
                    })?;
                    runner.frame(cx)?;
                    runner.click_menu(cx, "menu-view", "more-tests")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::LspBuffer {
                        update: fuzukue_proto::LspBuffer::Open { path, .. }, ..
                    } if path == "analysis.py"));
                    assert!(matches!(messages.try_recv()?, ClientMsg::ListTests));

                    let mut run = TestRun {
                        suite: "analysis".into(), revision: 1, state: TestState::Running,
                        passed: 0, failed: 0, failures: Vec::new(), output: "running\n".into(),
                    };
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::TestSuites {
                            suites: vec![TestSuite {
                                id: run.suite.clone(), kind: "custom".into(), cwd: ".".into(),
                                command: "python3 analysis.py".into(), unavailable: None,
                            }],
                            error: None,
                        }, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "tests-run-analysis".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::RunTests { suite, .. } if suite == "analysis"));

                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::TestRun { run: run.clone() }, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "tests-run-analysis".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::StopTests { suite } if suite == "analysis"));

                    run.revision += 1;
                    run.state = TestState::Failed;
                    run.failed = 1;
                    run.failures.push(TestFailure {
                        file: Some("analysis.py".into()), line: Some(8), message: "平均値".into(),
                    });
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::TestRun { run }, cx);
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "tests-failure-analysis-0".into() })?;
                    runner.frame(cx)?;
                    let caret = |cx: &mut AsyncApp| -> Result<_> {
                        runner.update(cx, |view, _, cx| view.source.caret_for_test(cx).unwrap())
                    };
                    assert_eq!(caret(cx)?.0, 7);
                    runner.key(cx, "End")?;
                    runner.frame(cx)?;
                    assert_eq!(caret(cx)?, (7, px(0.)));
                    runner.key(cx, "End")?;
                    runner.frame(cx)?;
                    assert_eq!(caret(cx)?, (7, px(0.)));
                    runner.key(cx, "Down")?;
                    runner.frame(cx)?;
                    assert_eq!(caret(cx)?, (8, px(0.)));
                    let pane_width = |cx: &mut AsyncApp| -> Result<_> {
                        runner.update(cx, |_, window, _| window.shot_element_bounds("source-pane").unwrap().size.width)
                    };
                    let docked = pane_width(cx)?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "source-dock-toggle".into() })?;
                    runner.frame(cx)?;
                    assert!(pane_width(cx)? > docked);
                    runner.click(cx, ClickTarget::Element { id: "source-dock-toggle".into() })?;
                    runner.frame(cx)?;
                    assert_eq!(pane_width(cx)?, docked);
                    assert_eq!(&*gpui_kit::component::locale(), "ja");
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn thread_info_toggle_shows_blockers_and_opens_the_goal_question() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner {
                    handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new(),
                };
                let outcome: Result<()> = async {
                    let settings = tempfile::tempdir()?;
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.appearance.isolate_shot_settings(settings.path().join("settings.toml"));
                        view.protocol = fuzukue_proto::GOAL_PAUSE_PROTOCOL;
                        view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                            github: Default::default(),
                            untitled: false,
                            id: "planner-fixture".into(), title: "引用の確認".into(), updated: 0,
                            archived: false, pinned: false, attached: vec![], auto_attached: vec![],
                            suggestions: vec![], math_macros: vec![], artifacts: vec![], claude_session: None,
                            partner_settings: None, worker_settings: None, agent_overrides: Default::default(),
                        }] }, cx);
                        view.on_event(ServerEvent::PlanChanged {
                            session: "planner-fixture".into(),
                            plan: fuzukue_proto::Plan {
                                goal: "原稿の引用を確認する".into(), done_condition: "出典との照合が済んでいる".into(),
                                status: fuzukue_proto::PlanStatus::Active, pace: Some(fuzukue_proto::GoalPace::Normal),
                                done: vec!["出典を整理した".into()],
                                next: vec![fuzukue_proto::PlanStep {
                                    routing: Default::default(), tier_reason: None, blocker: None,
                                    task: "引用を照合する".into(), agent: Some("prose".into()), reason: "版を確かめる".into(),
                                }],
                                rejected: vec![], question: None, verification: None, paused_by_user: false,
                            },
                            progress: Some(fuzukue_proto::GoalProgress::Paused { resume_at: Some(2_000_000_000) }),
                            parallel_limits: fuzukue_proto::GoalParallelLimits { normal: 3, economy: 1 },
                        }, cx);
                    })?;
                    runner.step(Step::WaitElement { id: "thread-goal-marker-planner-fixture".into() }, cx).await?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("thread-info-blocker-dot").is_none())?);
                    runner.update(cx, |view, _, cx| {
                        let (mut plan, _) = view.thread_info.plans["planner-fixture"].clone();
                        plan.status = fuzukue_proto::PlanStatus::WaitingForUser;
                        plan.question = Some("どの版と照合する？".into());
                        plan.next[0].blocker = Some(fuzukue_proto::StepBlocker {
                            reason: "版の指定を待っている".into(), needs: None,
                        });
                        view.on_event(ServerEvent::PlanChanged { session: "planner-fixture".into(), plan, progress: None, parallel_limits: Default::default() }, cx);
                    })?;
                    runner.step(Step::WaitElement { id: "thread-info-blocker-dot".into() }, cx).await?;
                    runner.step(Step::Click { id: Some("thread-info-toggle".into()), x: None, y: None }, cx).await?;
                    runner.step(Step::WaitElement { id: "thread-goal-question".into() }, cx).await?;
                    runner.step(Step::Click { id: Some("thread-replan".into()), x: None, y: None }, cx).await?;
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::Replan { session, .. } if session == "planner-fixture")));
                    runner.update(cx, |view, _, cx| {
                        let (mut plan, _) = view.thread_info.plans["planner-fixture"].clone();
                        plan.status = fuzukue_proto::PlanStatus::Active;
                        plan.question = None;
                        plan.paused_by_user = true;
                        view.on_event(ServerEvent::PlanChanged { session: "planner-fixture".into(), plan, progress: None, parallel_limits: Default::default() }, cx);
                    })?;
                    runner.step(Step::WaitElement { id: "thread-goal-pause".into() }, cx).await?;
                    runner.click(cx, ClickTarget::Element { id: "thread-goal-pause".into() })?;
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::ResumeNow { session } if session == "planner-fixture")));
                    runner.update(cx, |view, _, cx| {
                        let (mut plan, _) = view.thread_info.plans["planner-fixture"].clone();
                        plan.paused_by_user = false;
                        plan.next[0].blocker = None;
                        view.on_event(ServerEvent::PlanChanged { session: "planner-fixture".into(), plan, progress: None, parallel_limits: Default::default() }, cx);
                    })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("thread-info-blocker-dot").is_none())?);
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn git_panel_requests_the_selected_diff_and_commits_the_typed_message() {
        use fuzukue_proto::{GitFile, GitOutcome, GitRequest, GitStatus};
        const PATH: &str = "git-shot.txt";
        const MESSAGE: &str = "フィクスチャの変更を記録";

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let settings = tempfile::tempdir()?;
                    runner.update(cx, |view, window, cx| {
                        view.out = out.into();
                        view.appearance.isolate_shot_settings(settings.path().join("settings.toml"));
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.workspace_selected("git-fixture".into(), true, window, cx);
                        view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                            github: Default::default(),
                            untitled: false,
                            auto_attached: Vec::new(), suggestions: Vec::new(),
                            id: "thread".into(), title: "スレッド".into(), updated: 0, archived: false, pinned: false,
                            attached: vec![], math_macros: vec![], artifacts: vec![], claude_session: None, partner_settings: None, agent_overrides: Default::default(),
                    worker_settings: None,
                        }] }, cx);
                    })?;
                    let receive_git = || -> Result<(u64, GitRequest)> {
                        while let Ok(message) = messages.try_recv() {
                            if let ClientMsg::InWorkspace { id, request } = message {
                                assert_eq!(id, "git-fixture");
                                if let ClientMsg::Git { id, request } = *request {
                                    return Ok((id, request));
                                }
                            }
                        }
                        anyhow::bail!("Git request was not sent")
                    };
                    let (status_id, request) = receive_git()?;
                    assert!(matches!(request, GitRequest::Status));
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::GitResult { id: status_id, outcome: GitOutcome::Status {
                            status: GitStatus {
                                branch: Some("main".into()), upstream: None, ahead: 0, behind: 0, read_only: false,
                                files: vec![GitFile { path: PATH.into(), status: " M".into(), old_path: None }],
                            },
                            commits: vec![],
                        }}, cx);
                    })?;
                    runner.step(Step::Click { id: Some("thread-info-toggle".into()), x: None, y: None }, cx).await?;
                    runner.step(Step::Click { id: Some("git-toggle".into()), x: None, y: None }, cx).await?;
                    assert!(matches!(receive_git()?.1, GitRequest::Status));
                    runner.step(Step::Click { id: Some("git-file-0".into()), x: None, y: None }, cx).await?;
                    let (id, request) = receive_git()?;
                    assert!(matches!(request, GitRequest::Diff { path, staged: false } if path == PATH));
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::GitResult {
                            id, outcome: GitOutcome::Diff { text: "-before\n+after".into() },
                        }, cx);
                    })?;
                    runner.step(Step::WaitGit { path: None, diff: Some("+after".into()), commit: None }, cx).await?;
                    runner.step(Step::Click { id: Some("git-diff-close".into()), x: None, y: None }, cx).await?;
                    runner.step(Step::Click { id: Some("git-commit-open".into()), x: None, y: None }, cx).await?;
                    runner.step(Step::Type { text: MESSAGE.into() }, cx).await?;
                    runner.step(Step::Click { id: Some("git-commit-confirm".into()), x: None, y: None }, cx).await?;
                    assert!(matches!(receive_git()?.1, GitRequest::Commit { message } if message == MESSAGE));
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn paper_math_selection_copies_and_sends_the_quote() {
        use gpui_kit::base::TextSelection;
        use fuzukue_proto::{CommentView, DocItem, DocSection};

        const DECK: &str = "papers/levy-families";
        const CARD: &str = "thm-sphere-concentration";
        const INLINE: &str = r"S^n(1) \subset \mathbb{R}^{n+1}";
        const DISPLAY: &str = r"\alpha_{S^n(1)}(r) \le \sqrt{\pi/8}\, e^{-(n-1) r^2 / 2}.";
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                view.update(cx, |view, cx| {
                    view.switch_session("thread-1".into(), cx);
                    view.out = out.into();
                    view.on_event(ServerEvent::OpenPaper {
                        session: "thread-1".into(), deck: DECK.into(), card: Some(CARD.into()),
                    }, cx);
                    assert!(matches!(messages.try_recv().unwrap(), ClientMsg::SelectPaper { .. }));
                    view.on_event(ServerEvent::Workspace { tabs: vec![], deck: DECK.into() }, cx);
                    let statement = include_str!("../../../examples/lab/papers/levy-families/cards/thm/thm-sphere-concentration/statement.tex");
                    let statement = statement.replace(&format!("${INLINE}$"), &format!(r"\({INLINE}\)"));
                    let body = format!("前の段落。 $p$\n\n{}\n\n次の段落。 $q$", statement.trim());
                    let card = serde_json::from_value(serde_json::json!({
                        "id": CARD, "kind": "theorem", "title": "球面の集中", "status": "",
                        "path": "cards/thm-sphere-concentration/card.md", "body": body,
                        "links": [], "backlinks": [], "files": [], "external": false
                    })).unwrap();
                    view.on_event(ServerEvent::Cards {
                        deck: DECK.into(), cards: vec![card], diagnostics: vec![],
                    }, cx);
                    view.on_event(ServerEvent::Document {
                        title: "論文".into(), macros: vec![], math_macros: vec![], references: None,
                        sections: vec![DocSection { title: "節".into(), items: vec![DocItem::Card { id: CARD.into() }], prose: vec![], unmatched: false }],
                    }, cx);
                });
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle, view, target: None, output: PathBuf::new(),
                        read_only: false, seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        runner.step(Step::Size { width: 1440, height: 900 }, cx).await?;
                        runner.step(Step::WaitPaperCard { deck: DECK.into(), card: CARD.into(), text: None }, cx).await?;
                        let inline_id = format!("formula:{INLINE}");
                        runner.step(Step::DragFromElement { id: inline_id.clone(), dx: 110., dy: 0. }, cx).await?;
                        runner.frame(cx)?;
                        let quote_id = format!("quote-{CARD}");
                        let quote = runner.update(cx, |view, window, _| {
                            assert!(window.shot_element_bounds(&quote_id).is_some());
                            view.comment_popup.as_ref().unwrap().0.quote.clone().unwrap()
                        })?;
                        assert!(quote.starts_with(&format!("${INLINE}$ に")), "{quote:?}");

                        cx.update_window(runner.handle, |_, window, cx| {
                            assert_eq!(TextSelection::selected_text(window, cx).trim(), quote);
                            // The Linux headless platform has no system clipboard.
                            // Check the Copy payload above and dispatch its key binding here.
                            let copy = Keystroke::parse_with_os("ctrl-c", gpui_kit::OperatingSystem::Linux).unwrap();
                            assert!(window.dispatch_keystroke(copy, cx));
                        })?;

                        let [inline, display, previous_marker, next_marker] = runner.update(cx, |_, window, _| {
                            [&inline_id, &format!("formula:{DISPLAY}"), "formula:p", "formula:q"]
                                .map(|id| window.shot_element_bounds(id).unwrap())
                        })?;
                        let previous = point(previous_marker.left() - px(64.), previous_marker.center().y);
                        let next = point(next_marker.left() - px(16.), next_marker.center().y);
                        let following = inline.center() + point(px(110.), px(0.));
                        let inline_left = inline.origin + point(inline.size.width / 4., inline.size.height / 2.);
                        let inline_right = inline_left + point(inline.size.width / 2., px(0.));
                        let inline_tex = format!("${INLINE}$");
                        let display_tex = format!("$$\n{DISPLAY}\n$$");
                        runner.update(cx, |view, _, cx| {
                            view.comment_popup = None;
                            cx.notify();
                        })?;
                        runner.frame(cx)?;
                        // Release before any selected range has been painted.
                        cx.update_window(runner.handle, |_, window, cx| {
                            for event in [
                                PlatformInput::MouseDown(MouseDownEvent {
                                    position: inline.center(),
                                    click_count: 1,
                                    ..Default::default()
                                }),
                                PlatformInput::MouseMove(MouseMoveEvent {
                                    position: following,
                                    pressed_button: Some(gpui_kit::MouseButton::Left),
                                    ..Default::default()
                                }),
                                PlatformInput::MouseUp(MouseUpEvent {
                                    position: following,
                                    click_count: 1,
                                    ..Default::default()
                                }),
                            ] {
                                window.dispatch_event(event, cx);
                            }
                        })?;
                        runner.update(cx, |view, _, _| {
                            assert_eq!(
                                view.comment_popup.as_ref().and_then(|(target, _)| target.quote.as_deref()),
                                Some(quote.as_str()),
                            );
                        })?;

                        for (a, b, fragments) in [
                            (inline_right, following, [&inline_tex, "に"]),
                            (inline_left, previous, [&inline_tex, "段落。"]),
                            (inline_right, next, [&inline_tex, "次の"]),
                            (display.center(), next, [&display_tex, "次の"]),
                            (display.center(), following, [&display_tex, "一様測度"]),
                        ] {
                            let mut first = None;
                            for (from, to) in [(a, b), (b, a)] {
                                runner.update(cx, |view, _, cx| {
                                    view.comment_popup = None;
                                    cx.notify();
                                })?;
                                runner.frame(cx)?;
                                runner.drag(cx, from, to)?;
                                runner.frame(cx)?;
                                let selected = runner.update(cx, |view, window, cx| {
                                    let selected = TextSelection::selected_text(window, cx);
                                    assert_eq!(
                                        view.comment_popup.as_ref().and_then(|(target, _)| target.quote.as_deref()),
                                        Some(selected.trim()),
                                        "{from:?} -> {to:?}, expected {fragments:?}",
                                    );
                                    selected
                                })?;
                                for fragment in fragments {
                                    assert!(selected.contains(fragment), "{from:?} -> {to:?}: {selected:?} lacks {fragment:?}");
                                }
                                if let Some(first) = first.as_ref() {
                                    assert_eq!(&selected, first);
                                } else {
                                    first = Some(selected);
                                }
                            }
                        }

                        for (position, count, expected, paragraph) in [
                            (inline.center(), 2, inline_tex.as_str(), false),
                            (inline.center(), 3, inline_tex.as_str(), true),
                            (display.center(), 2, display_tex.as_str(), false),
                            (display.center(), 3, display_tex.as_str(), false),
                        ] {
                            runner.update(cx, |view, _, cx| {
                                view.comment_popup = None;
                                cx.notify();
                            })?;
                            runner.frame(cx)?;
                            cx.update_window(runner.handle, |_, window, cx| {
                                window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                                    position, click_count: count, ..Default::default()
                                }), cx);
                                window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                                    position, click_count: count, ..Default::default()
                                }), cx);
                            })?;
                            runner.frame(cx)?;
                            runner.update(cx, |_, window, cx| {
                                let selected = TextSelection::selected_text(window, cx);
                                if paragraph {
                                    assert!(selected.starts_with(expected) && selected.contains("一様測度"), "{selected:?}");
                                    assert!(!selected.contains(DISPLAY), "{selected:?}");
                                } else {
                                    assert_eq!(selected.trim(), expected);
                                }
                            })?;
                        }
                        runner.update(cx, |view, _, cx| {
                            view.comment_popup = None;
                            cx.notify();
                        })?;
                        runner.step(Step::DragFromElement { id: inline_id, dx: 110., dy: 0. }, cx).await?;
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: quote_id })?;
                        runner.frame(cx)?;
                        runner.update(cx, |view, _, _| {
                            assert_eq!(view.comment_target.as_ref().unwrap().quote.as_deref(), Some(quote.as_str()));
                        })?;
                        runner.step(Step::Type { text: "選択した数式へのコメント".into() }, cx).await?;
                        runner.click(cx, ClickTarget::Element { id: "add-comment".into() })?;
                        let ClientMsg::AddComment { card, text, quote: sent, source, .. } = messages.try_recv()?
                        else { bail!("the quoted draft did not send a comment") };
                        assert_eq!(card, CARD);
                        assert_eq!(text, "選択した数式へのコメント");
                        assert_eq!(sent.as_deref(), Some(quote.as_str()));
                        assert_eq!(
                            source.as_ref().unwrap().text,
                            quote.replacen(&inline_tex, &format!(r"\({INLINE}\)"), 1),
                        );

                        runner.update(cx, |view, _, cx| {
                            view.on_event(ServerEvent::Comments {
                                deck: DECK.into(),
                                items: vec![CommentView {
                                    card, text, quote: sent, proof: Some(false), source, created: 0,
                                }],
                            }, cx);
                        })?;
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: "send-comments".into() })?;
                        assert!(matches!(
                            messages.try_recv()?,
                            ClientMsg::SendComments { deck, .. } | ClientMsg::ApplyComments { deck, .. }
                                if deck == DECK
                        ));

                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn composer_keys_and_drafts_follow_workspace_and_thread_changes() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        runner.update(cx, |view, window, cx| {
                            view.workspace_selected("lab".into(), false, window, cx);
                            view.switch_session("thread-1".into(), cx);
                        })?;
                        runner.frame(cx)?;
                        let scenario: Scenario = serde_json::from_str(
                            include_str!("../../../tests/shots/composer-assist.json"),
                        )?;
                        for step in scenario.steps.into_iter().filter(|step| matches!(
                            step, Step::Click { .. } | Step::Type { .. }
                                | Step::Key { .. } | Step::AssertComposerText { .. }
                        )) {
                            runner.step(step, cx).await?;
                        }

                        let assert_text = |text: &str| Step::AssertComposerText { text: text.into() };
                        runner.update(cx, |view, _, cx| view.switch_session("thread-2".into(), cx))?;
                        runner.step(assert_text(""), cx).await?;
                        runner.update(cx, |view, window, cx| {
                            view.workspace_selected("other".into(), false, window, cx);
                            view.switch_session("thread-1".into(), cx);
                        })?;
                        runner.step(assert_text(""), cx).await?;
                        runner.update(cx, |view, window, cx| {
                            view.workspace_selected("lab".into(), false, window, cx);
                            view.switch_session("thread-1".into(), cx);
                        })?;
                        runner.step(assert_text("- a\n- b\n"), cx).await?;

                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                        for (text, offset, key, selected) in [
                            ("- a", 3, "shift-enter", 4..4),
                            ("abcd\n0123", 7, "down", 7..7),
                            ("abcd\n0123", 2, "shift-up", 0..2),
                            ("abcd\n0123", 7, "shift-down", 7..9),
                        ] {
                            runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                                input.set_value(text, window, cx);
                                input.set_selected_range(offset..offset, cx);
                            }))?;
                            runner.frame(cx)?;
                            runner.key(cx, key)?;
                            let range = runner.update(cx, |view, _, cx| view.input.read(cx).selected_range())?;
                            ensure!(range == selected, "selection after {key}: {range:?}");
                            if key == "shift-enter" {
                                runner.step(assert_text("- a\n"), cx).await?;
                            }
                        }
                        let wrapped = "wrap ".repeat(150);
                        runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                            input.set_value(wrapped.as_str(), window, cx);
                        }))?;
                        runner.frame(cx)?;
                        runner.key(cx, "down")?;
                        let row = |cx: &mut AsyncApp| runner.update(cx, |view, _, cx| {
                            view.input.read(cx).visual_cursor_row(cx)
                        });
                        ensure!(matches!(row(cx)?, Some((1, count)) if count > 2),
                            "Down should move to the second visual row");
                        runner.update(cx, |view, _, cx| view.input.update(cx, |input, cx| {
                            let caret = wrapped.len() - 1;
                            input.set_selected_range(caret..caret, cx);
                        }))?;
                        runner.frame(cx)?;
                        runner.key(cx, "down")?;
                        let caret = runner.update(cx, |view, _, cx| view.input.read(cx).cursor())?;
                         ensure!(caret == wrapped.len() - 1, "Down on the last wrapped row should preserve the caret");
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn clicked_message_keeps_transcript_keyboard_scrolling_after_virtualization() {
        use fuzukue_proto::Actor;
        use gpui_kit::KeyDownEvent;

        const SESSION: &str = "clicked-message-keys";
        const BUBBLE: &str = "chat-message-bubble-95";
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                cx.spawn(async move |cx| {
                    let runner = Runner {
                        handle, view, target: None, output: PathBuf::new(),
                        read_only: false, seen: HashMap::new(),
                    };
                    let outcome: Result<()> = (|| {
                        let delta = |index, text| {
                            let (actor, item) = if index == 95 {
                                (Actor::Claude, "reply:95".into())
                            } else {
                                (Actor::User, format!("user:{index}"))
                            };
                            ServerEvent::TextDelta { session: SESSION.into(), item, actor, text }
                        };
                        runner.update(cx, |view, _, cx| {
                            view.switch_session(SESSION.into(), cx);
                            for index in 0..96 {
                                view.on_event(delta(index, format!("Message {index}")), cx);
                            }
                            view.chats[SESSION].scroll_to_end();
                        })?;
                        runner.frame(cx)?;
                        let bubble = runner.update(cx, |_, window, _| window.shot_element_bounds(BUBBLE))?
                            .context("last message is missing from the first frame")?;
                        runner.click(cx, ClickTarget::Coordinates {
                            x: f32::from(bubble.left() + px(8.)), y: f32::from(bubble.center().y),
                        })?;
                        runner.frame(cx)?;
                        let message_focus = runner.update(cx, |view, window, cx| {
                            window.focused(cx).filter(|focus| {
                                focus != &view.chat_focus && view.chat_focus.contains(focus, window)
                            })
                        })?.context("click must focus the Markdown inside the transcript")?;
                        runner.update(cx, |view, _, cx| {
                            view.on_event(delta(95, " extended".into()), cx);
                        })?;
                        runner.frame(cx)?;
                        ensure!(runner.update(cx, |_, window, _| message_focus.is_focused(window))?,
                            "updating a visible message must keep its Markdown focused");
                        drop(message_focus);

                        let scroll = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| {
                            -view.chats[SESSION].scroller.scroll_px_offset_for_scrollbar().y
                        });
                        let held_key = |cx: &mut AsyncApp, key: &'static str| -> Result<()> {
                            let keystroke = Keystroke::parse(key).map_err(|err| anyhow!("{err}"))?;
                            cx.update_window(handle, |_, window, cx| {
                                window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent {
                                    keystroke, is_held: true, prefer_character_input: false,
                                }), cx);
                            })?;
                            runner.frame(cx)?;
                            ensure!(runner.update(cx, |view, window, cx| view.chat_focus.contains_focused(window, cx))?,
                                "{key} lost the transcript focus path");
                            Ok(())
                        };
                        let visible = |cx: &mut AsyncApp| runner.update(cx, |_, window, _| {
                            window.shot_element_bounds(BUBBLE).is_some()
                        });
                        runner.key(cx, "up")?;
                        runner.frame(cx)?;
                        for _ in 0..128 {
                            if !visible(cx)? {
                                break;
                            }
                            held_key(cx, "up")?;
                        }
                        ensure!(!visible(cx)?, "repeated Up must move the clicked message out of the rendered range");
                        runner.update(cx, |view, _, cx| {
                            view.on_event(delta(96, "New message while scrolling".into()), cx);
                        })?;
                        runner.frame(cx)?;
                        for (key, direction) in [("up", -1.), ("down", 1.), ("pageup", -1.), ("pagedown", 1.)] {
                            for _ in 0..3 {
                                let before = scroll(cx)?;
                                held_key(cx, key)?;
                                ensure!((scroll(cx)? - before) * direction > px(0.),
                                    "repeated {key} must scroll after the clicked row is gone");
                            }
                        }
                        for _ in 0..2 {
                            held_key(cx, "home")?;
                            ensure!(scroll(cx)? == px(0.), "Home must reach the top");
                            held_key(cx, "end")?;
                            ensure!(runner.update(cx, |view, _, _| view.chats[SESSION].scroller.is_following_tail())?,
                                "End must reach the tail");
                        }
                        Ok(())
                    })();
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn composer_arrows_scroll_only_at_visual_edges_and_keep_focus() {
        use gpui_kit::{EntityInputHandler as _, Focusable as _};
        use fuzukue_proto::Actor;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                cx.spawn(async move |cx| {
                    let runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        runner.update(cx, |view, _, cx| {
                            let session = "keyboard-thread".to_owned();
                            view.switch_session(session.clone(), cx);
                            for i in 0..32 {
                                view.on_event(ServerEvent::TextDelta {
                                    session: session.clone(),
                                    item: format!("user:{i}"),
                                    actor: Actor::User,
                                    text: format!("Message {i}"),
                                }, cx);
                            }
                            view.chats[&view.active].scroller.scroll_to(gpui_kit::ListOffset::default());
                        })?;
                        // Visit each part of this fixture before comparing exact pixel offsets.
                        // Offscreen estimates are allowed to change when a row first comes into view.
                        runner.frame(cx)?;
                        for index in [8, 16] {
                            runner.update(cx, |view, _, cx| {
                                view.chats[&view.active].scroller.scroll_to(gpui_kit::ListOffset {
                                    item_ix: index,
                                    offset_in_item: px(0.),
                                });
                                cx.notify();
                            })?;
                            runner.frame(cx)?;
                        }
                        runner.update(cx, |view, _, cx| {
                            view.chats[&view.active].scroll_to_end();
                            cx.notify();
                        })?;
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                        runner.frame(cx)?;
                        let thread_focused = |cx: &mut AsyncApp| runner.update(cx, |view, window, _| {
                            view.chat_focus.is_focused(window)
                        });
                        let thread_has_frame = |cx: &mut AsyncApp| runner.update(cx, |view, window, _| {
                            let bounds = window.shot_element_bounds(&format!("chat-{}", view.active))
                                .context("transcript bounds are missing")?
                                .scale(window.scale_factor());
                            Ok::<_, anyhow::Error>(window.painted_quads().iter().any(|quad| {
                                quad.border_color.a > 0.
                                    && quad.border_widths != Default::default()
                                    && (quad.bounds.left().0 - bounds.left().0).abs() < 1.
                                    && (quad.bounds.right().0 - bounds.right().0).abs() < 1.
                                    && (quad.bounds.top().0 - bounds.top().0).abs() < 1.
                                    && (quad.bounds.bottom().0 - bounds.bottom().0).abs() < 1.
                            }))
                        }).and_then(|result| result);
                        let composer_focused = |cx: &mut AsyncApp| runner.update(cx, |view, window, cx| {
                            view.input.read(cx).focus_handle(cx).is_focused(window)
                        });
                        let caret = |cx: &mut AsyncApp| runner.update(cx, |view, _, cx| view.input.read(cx).cursor());
                        let edges = |cx: &mut AsyncApp| runner.update(cx, |view, _, cx| {
                            let input = view.input.read(cx);
                            (input.vertical_arrow_at_edge(false, cx), input.vertical_arrow_at_edge(true, cx))
                        });
                        let scroll = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| {
                            let list = &view.chats[&view.active].scroller;
                            (-list.scroll_px_offset_for_scrollbar().y, list.is_following_tail())
                        });
                        let (line, page) = runner.update(cx, |view, window, _| {
                            let line = window.line_height();
                            (line, view.chats[&view.active].scroller.viewport_bounds().size.height.max(line))
                        })?;
                        let (bottom, following) = scroll(cx)?;
                        ensure!(following && bottom > page, "fixture must scroll at least one page");
                        ensure!(edges(cx)? == (true, true), "empty composer must have both edges");
                        ensure!(!thread_has_frame(cx)?, "unfocused transcript must be frameless");
                        runner.click(cx, ClickTarget::Element { id: "chat-keyboard-thread".into() })?;
                        runner.frame(cx)?;
                        ensure!(thread_focused(cx)?, "click must focus the transcript");
                        ensure!(!thread_has_frame(cx)?, "mouse focus must leave the transcript frameless");
                        runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                        runner.frame(cx)?;
                        runner.key(cx, "up")?;
                        runner.frame(cx)?;
                        let (y, following) = scroll(cx)?;
                        ensure!(composer_focused(cx)? && !following, "Up must keep composer focus and leave the tail");
                        ensure!(!thread_has_frame(cx)?, "keyboard focus must leave the transcript frameless");
                        ensure!((y - (bottom - line)).abs() < px(1.),
                            "Up scrolled from {bottom:?} to {y:?}, expected a line of {line:?}");
                        runner.wait_for(cx, "jump to latest", TIMEOUT_MS, |_, window| {
                            window.shot_element_bounds("jump-to-latest").is_some()
                        }).await?;
                        for (key, expected) in [
                            ("up", bottom - line * 2.),
                            ("down", bottom - line),
                            ("pageup", bottom - line - page),
                            ("pagedown", bottom - line),
                            ("home", px(0.)),
                            ("end", bottom),
                            ("up", bottom - line),
                        ] {
                            runner.key(cx, key)?;
                            runner.frame(cx)?;
                            let y = scroll(cx)?.0;
                            ensure!(composer_focused(cx)? && (y - expected).abs() < px(1.),
                                "{key} scrolled to {y:?}, expected {expected:?}");
                        }
                        runner.key(cx, "down")?;
                        runner.frame(cx)?;
                        ensure!(composer_focused(cx)? && (scroll(cx)?.0 - bottom).abs() < px(1.),
                            "Down must return to the tail with the composer still focused");
                        runner.click(cx, ClickTarget::Element { id: "chat-keyboard-thread".into() })?;
                        runner.frame(cx)?;
                        ensure!(thread_focused(cx)?, "click must focus the transcript");
                        runner.key(cx, "down")?;
                        runner.frame(cx)?;
                        ensure!(composer_focused(cx)? && scroll(cx)?.1, "Down from the focused transcript's tail must focus the composer");

                        runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                            input.set_value("First line\nSecond line", window, cx);
                            input.set_selected_range(2..2, cx);
                        }))?;
                        runner.frame(cx)?;
                        let before = scroll(cx)?;
                        for (key, edge) in [("down", (false, true)), ("up", (true, false))] {
                            runner.key(cx, key)?;
                            runner.frame(cx)?;
                            ensure!(edges(cx)? == edge && composer_focused(cx)? && scroll(cx)? == before,
                                "{key} must move between draft lines without scrolling the transcript");
                        }

                        let wrapped = "wrap ".repeat(150);
                        runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                            input.set_value(wrapped.as_str(), window, cx);
                            input.set_selected_range(2..2, cx);
                        }))?;
                        runner.frame(cx)?;
                        ensure!(edges(cx)? == (true, false), "caret is inside the first wrapped row");
                        let before = scroll(cx)?;
                        for key in ["alt-up", "ctrl-up", "shift-up"] {
                            runner.key(cx, key)?;
                            ensure!(composer_focused(cx)?, "{key} must stay in the composer");
                        }
                        ensure!(edges(cx)? == (false, false), "selection must block boundary scrolling");
                        runner.key(cx, "up")?;
                        ensure!(composer_focused(cx)?, "Up with a selection must stay in the composer");
                        runner.update(cx, |view, _, cx| view.input.update(cx, |input, cx| {
                            let caret = wrapped.len() - 1;
                            input.set_selected_range(caret..caret, cx);
                        }))?;
                        ensure!(edges(cx)? == (false, true), "caret is inside the last wrapped row");
                        runner.update(cx, |view, _, cx| view.input.update(cx, |input, cx| {
                            input.set_selected_range(2..2, cx);
                        }))?;
                        runner.key(cx, "down")?;
                        runner.frame(cx)?;
                        ensure!(edges(cx)? == (false, false), "second of several wrapped rows has no edge");
                        runner.key(cx, "up")?;
                        ensure!(composer_focused(cx)? && edges(cx)?.0, "Up from the second wrapped row must move the caret");
                        ensure!(scroll(cx)? == before, "navigation inside the wrapped draft scrolled the transcript");
                        let first_caret = caret(cx)?;
                        runner.key(cx, "up")?;
                        runner.frame(cx)?;
                        let after_up = scroll(cx)?.0;
                        ensure!(after_up < before.0 && composer_focused(cx)? && caret(cx)? == first_caret,
                            "Up on the first wrapped row must scroll and preserve the focused caret");
                        let last_caret = wrapped.len() - 1;
                        runner.update(cx, |view, _, cx| view.input.update(cx, |input, cx| {
                            input.set_selected_range(last_caret..last_caret, cx);
                        }))?;
                        runner.frame(cx)?;
                        runner.key(cx, "down")?;
                        runner.frame(cx)?;
                        ensure!(scroll(cx)?.0 > after_up && composer_focused(cx)? && caret(cx)? == last_caret,
                            "Down on the last wrapped row must scroll and preserve the focused caret");

                        runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                            input.set_value("", window, cx);
                            input.replace_and_mark_text_in_range(None, "あ", Some(1..1), window, cx);
                        }))?;
                        runner.frame(cx)?;
                        let before = scroll(cx)?;
                        for key in ["up", "down"] {
                            runner.key(cx, key)?;
                            runner.frame(cx)?;
                            let marked = runner.update(cx, |view, window, cx| {
                                crate::composing(&view.input, window, cx)
                            })?;
                            ensure!(marked && composer_focused(cx)? && scroll(cx)? == before,
                                "{key} during preedit must preserve composition, focus, and scroll");
                        }
                        runner.update(cx, |view, window, cx| view.input.update(cx, |input, cx| {
                            input.unmark_text(window, cx);
                            input.set_value("draft", window, cx);
                            input.set_selected_range(2..2, cx);
                        }))?;
                        runner.frame(cx)?;
                        let before = scroll(cx)?.0;
                        for (key, direction) in [("up", -1.), ("down", 1.)] {
                            let previous = scroll(cx)?.0;
                            runner.key(cx, key)?;
                            runner.frame(cx)?;
                            ensure!((scroll(cx)?.0 - previous) * direction > px(0.)
                                && composer_focused(cx)? && caret(cx)? == 2,
                                "{key} in a single-line draft must scroll and preserve the focused caret");
                        }
                        ensure!((scroll(cx)?.0 - before).abs() < px(1.), "opposite arrows must return to the same offset");
                        for key in ["enter", "escape"] {
                            runner.click(cx, ClickTarget::Element { id: "chat-keyboard-thread".into() })?;
                            runner.frame(cx)?;
                            ensure!(thread_focused(cx)?, "click must focus the transcript");
                            runner.key(cx, key)?;
                            ensure!(composer_focused(cx)?, "{key} must return focus to the composer");
                        }
                        runner.click(cx, ClickTarget::Element { id: "chat-keyboard-thread".into() })?;
                        runner.frame(cx)?;
                        runner.type_text(cx, "X")?;
                        let text = runner.update(cx, |view, _, cx| view.input.read(cx).value())?;
                        ensure!(composer_focused(cx)? && text.as_ref() == "drXaft",
                            "typing in the thread must insert at the composer's caret");
                        runner.update(cx, |view, window, cx| {
                            view.protocol = fuzukue_proto::SLASH_COMMAND_PROTOCOL;
                            view.threads_loaded = true;
                            view.input.update(cx, |input, cx| input.set_value("/", window, cx));
                            cx.notify();
                        })?;
                        runner.frame(cx)?;
                        let before = scroll(cx)?;
                        for (key, index) in [("down", 1), ("up", 0)] {
                            runner.key(cx, key)?;
                            runner.frame(cx)?;
                            let selected = runner.update(cx, |view, _, _| view.composer_markdown.command_palette.selected)?;
                            ensure!(selected == index && composer_focused(cx)? && scroll(cx)? == before,
                                "{key} must navigate suggestions without scrolling the transcript");
                        }
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn chat_code_button_and_selection_copy_literal_text() {
        use fuzukue_proto::Actor;
        use gpui_kit::{ClipboardItem, ShotClipboard};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                cx.set_global(ShotClipboard::default());
                let (handle, view) = open_workbench_at_size(cx, 860., 800.).unwrap();
                cx.spawn(async move |cx| {
                    let runner = Runner {
                        handle, view, target: None, output: PathBuf::new(),
                        read_only: false, seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        let expected = format!("\t{}\n\n\tlast  \n", "echo $NAME ".repeat(16));
                        let source = format!("```sh\r\n{}\r\n```\r\n\nEnd of message", expected.replace('\n', "\r\n"));
                        runner.update(cx, |view, _, cx| {
                            let session = "code-copy-fixture".to_owned();
                            view.switch_session(session.clone(), cx);
                            view.on_event(ServerEvent::TextDelta {
                                session, item: "reply:code".into(), actor: Actor::Claude,
                                text: source.clone(),
                            }, cx);
                        })?;
                        runner.wait_for(cx, "fenced code", TIMEOUT_MS, |_, window| {
                            window.shot_element_bounds("codeblock-0").is_some()
                        }).await?;
                        let block = runner.update(cx, |_, window, _| window.shot_element_bounds("codeblock-0"))?
                            .context("the fenced block should be rendered")?;
                        ensure!(block.size.height > px(125.), "expected the long line to wrap: {block:?}");
                        runner.click(cx, ClickTarget::Element { id: "chat-code-copy".into() })?;
                        let clipboard = |cx: &mut AsyncApp| {
                            runner.update(cx, |_, _, cx| cx.read_from_clipboard().and_then(|item| item.text()))
                        };
                        ensure!(clipboard(cx)?.as_deref() == Some(expected.as_str()), "unexpected button copy: {:?}", clipboard(cx)?);

                        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("sentinel".into())));
                        runner.frame(cx)?;
                        let position = point(block.left() + px(36.), block.top() + px(46.));
                        cx.update_window(handle, |_, window, cx| {
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position, ..Default::default()
                            }), cx);
                            window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                                position, click_count: 3, ..Default::default()
                            }), cx);
                            window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                                position, click_count: 3, ..Default::default()
                            }), cx);
                        })?;
                        runner.frame(cx)?;
                        runner.key(cx, "ctrl-c")?;
                        ensure!(clipboard(cx)?.as_deref() == Some(expected.as_str()), "unexpected selection copy: {:?}", clipboard(cx)?);

                        let bubble = runner.update(cx, |_, window, _| {
                            window.shot_element_bounds("chat-message-bubble-0")
                        })?.context("the message should be rendered")?;
                        runner.drag(
                            cx,
                            point(block.left() + px(16.), position.y),
                            point(bubble.left() + px(230.), bubble.bottom() - px(10.)),
                        )?;
                        runner.frame(cx)?;
                        runner.key(cx, "ctrl-c")?;
                        let mixed = format!("{expected}\nEnd of message");
                        ensure!(clipboard(cx)?.as_deref() == Some(mixed.as_str()), "unexpected mixed copy: {:?}", clipboard(cx)?);

                        runner.key(cx, "ctrl-a")?;
                        runner.frame(cx)?;
                        runner.key(cx, "ctrl-c")?;
                        ensure!(clipboard(cx)?.as_deref() == Some(source.as_str()), "unexpected full message copy: {:?}", clipboard(cx)?);
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn chat_quote_reply_uses_the_transcript_selection_and_preserves_the_draft() {
        use fuzukue_proto::Actor;
        use gpui_kit::{base::TextSelection, Focusable as _};

        const PILL: &str = "chat-quote-reply";
        const QUOTE: &str = "> First paragraph\n> \n> list one\n> list two\n> \n> ~~~\n> let n = 1;\n> ~~~\n> \n> Last message";
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                cx.spawn(async move |cx| {
                    let runner = Runner {
                        handle, view, target: None, output: PathBuf::new(),
                        read_only: false, seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        runner.update(cx, |view, window, cx| {
                            let session = "quote-fixture".to_owned();
                            view.switch_session(session.clone(), cx);
                            for index in 0..32 {
                                let text = match index {
                                    30 => "**First** paragraph\n\n- list one\n- list two\n\n```rust\nlet n = 1;\n```".into(),
                                    31 => "Last message".into(),
                                    _ => format!("Earlier message {index}"),
                                };
                                view.on_event(ServerEvent::TextDelta {
                                    session: session.clone(), item: format!("reply:{index}"),
                                    actor: Actor::Claude, text,
                                }, cx);
                            }
                            view.chats[&view.active].scroll_to_end();
                            view.input.update(cx, |input, cx| {
                                input.set_value("前後", window, cx);
                                let caret = "前".len();
                                input.set_selected_range(caret..caret, cx);
                            });
                        })?;
                        runner.wait_for(cx, "last transcript message", TIMEOUT_MS, |_, window| {
                            window.shot_element_bounds("chat-message-bubble-31").is_some()
                        }).await?;
                        let select = |cx: &mut AsyncApp| -> Result<()> {
                            let (start, end) = runner.update(cx, |_, window, _| {
                                let first = window.shot_element_bounds("chat-message-bubble-30")
                                    .context("first selected message is missing")?;
                                let last = window.shot_element_bounds("chat-message-bubble-31")
                                    .context("last selected message is missing")?;
                                anyhow::Ok((
                                    point(first.left() + px(1.), first.top() + px(8.)),
                                    point(last.left() + px(130.), last.center().y),
                                ))
                            })??;
                            runner.drag(cx, start, end)?;
                            runner.frame(cx)?;
                            runner.frame(cx)
                        };
                        let pill_visible = |_: &Workbench, window: &Window| window.shot_element_bounds(PILL).is_some();
                        let pill_hidden = |_: &Workbench, window: &Window| window.shot_element_bounds(PILL).is_none();
                        select(cx)?;
                        let (copied, quoted, bounds) = runner.update(cx, |_, window, cx| {
                            let region = gpui_kit::ElementId::from("chat-quote-fixture");
                            (
                                TextSelection::selected_text(window, cx),
                                TextSelection::quote_text(&region, window, cx),
                                TextSelection::quote_bounds(&region, window, cx),
                            )
                        })?;
                        ensure!(bounds.is_some(), "selection should expose its ending caret");
                        ensure!(quoted.trim() == "First paragraph\n\nlist one\nlist two\n\n~~~\nlet n = 1;\n~~~\n\nLast message",
                            "unexpected quote selection: {quoted:?}");
                        ensure!(copied.trim() == "First paragraph\nlist one\nlist two\nlet n = 1;\n\nLast message",
                            "unexpected clipboard selection: {copied:?}");
                        runner.wait_for(cx, "quote action", TIMEOUT_MS, pill_visible).await?;
                        runner.key(cx, "ctrl-c")?;
                        runner.frame(cx)?;
                        ensure!(runner.update(cx, |view, window, _| pill_visible(view, window))?, "Copy should keep the selection");

                        runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                        runner.wait_for(cx, "a click outside to clear the quote action", TIMEOUT_MS, pill_hidden).await?;
                        runner.update(cx, |view, _, cx| view.input.update(cx, |input, cx| {
                            let caret = "前".len();
                            input.set_selected_range(caret..caret, cx);
                        }))?;
                        select(cx)?;
                        runner.wait_for(cx, "quote action before Escape", TIMEOUT_MS, pill_visible).await?;
                        runner.key(cx, "escape")?;
                        runner.wait_for(cx, "Escape to dismiss the quote action", TIMEOUT_MS, pill_hidden).await?;
                        select(cx)?;
                        runner.wait_for(cx, "quote action before scrolling", TIMEOUT_MS, pill_visible).await?;
                        runner.update(cx, |view, _, cx| {
                            view.chats[&view.active].scroller.scroll_to(gpui_kit::ListOffset::default());
                            cx.notify();
                        })?;
                        runner.wait_for(cx, "scrolling to hide the quote action", TIMEOUT_MS, pill_hidden).await?;
                        runner.update(cx, |view, _, cx| {
                            view.chats[&view.active].scroll_to_end();
                            cx.notify();
                        })?;
                        runner.frame(cx)?;

                        select(cx)?;
                        runner.wait_for(cx, "quote action after returning to the tail", TIMEOUT_MS, pill_visible).await?;
                        runner.click(cx, ClickTarget::Element { id: PILL.into() })?;
                        runner.frame(cx)?;
                        let (value, caret, focus) = runner.update(cx, |view, window, cx| {
                            let input = view.input.read(cx);
                            (input.value(), input.cursor(), input.focus_handle(cx).is_focused(window))
                        })?;
                        ensure!(value.as_ref() == format!("前\n\n{QUOTE}\n\n\n後"), "unexpected quoted draft: {value:?}");
                        ensure!(caret == format!("前\n\n{QUOTE}\n\n").len() && focus, "the caret should be on the empty reply line");
                        runner.type_text(cx, "Reply")?;
                        let value = runner.update(cx, |view, _, cx| view.input.read(cx).value())?;
                        ensure!(value.as_ref() == format!("前\n\n{QUOTE}\n\nReply\n後"), "unexpected reply draft: {value:?}");

                        for from_menu in [false, true] {
                            runner.update(cx, |view, window, cx| {
                                view.input.update(cx, |input, cx| input.clean(window, cx));
                            })?;
                            runner.frame(cx)?;
                            select(cx)?;
                            if from_menu {
                                runner.click_menu(cx, "menu-edit", "menu-quote-reply")?;
                            } else {
                                runner.key(cx, "Ctrl+Shift+Q")?;
                            }
                            runner.frame(cx)?;
                            runner.update(cx, |view, window, cx| {
                                let input = view.input.read(cx);
                                let value = input.value();
                                ensure!(value.as_ref() == format!("{QUOTE}\n\n"), "unexpected draft from menu={from_menu}: {value:?}");
                                ensure!(input.cursor() == value.len() && input.focus_handle(cx).is_focused(window),
                                    "the composer should be focused with its caret at the end");
                                Ok(())
                            })??;
                        }
                        Ok(())
                    }.await;
                    result.send(outcome).unwrap();
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn image_upload_stays_in_the_composer_until_sent() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                view.update(cx, |view, cx| {
                    view.switch_session("attachment-fixture".into(), cx);
                    view.out = out.into();
                    view.on_event(ServerEvent::ChatCapabilities {
                        effort_levels: vec![], mail_restricted: false, mcp_tools: vec![], folder_attachments: true,
                    }, cx);
                });
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle, view, target: None, output: PathBuf::new(),
                        read_only: false, seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        let dir = tempfile::tempdir()?;
                        let image_path = dir.path().join("screenshot.png");
                        image::RgbaImage::from_pixel(360, 240, image::Rgba([30, 118, 190, 255]))
                            .save(&image_path)?;
                        let session = "attachment-fixture";
                        runner.update(cx, |view, _, cx| view.add_attachments(session.into(), vec![image_path], cx))?;
                        let started = Instant::now();
                        let id = loop {
                            runner.frame(cx)?;
                            if let Some(id) = messages.try_iter().find_map(|message| match message {
                                ClientMsg::UploadAttachment { id, .. } => Some(id),
                                _ => None,
                            }) {
                                break id;
                            }
                            ensure!(started.elapsed() < Duration::from_millis(TIMEOUT_MS), "upload did not start");
                            pause(cx, 30).await;
                        };
                        let draft = format!("chat-draft-image-{id}");
                        let size = gpui_kit::size(px(240.), px(160.));
                        runner.update(cx, |view, _, cx| view.on_event(ServerEvent::AttachmentUploaded {
                            id, path: Some("inbox/screenshot.png".into()), error: None,
                            proposal: Some(fuzukue_proto::InboxProposal {
                                to: "inbox".into(), title: None, reference_id: None,
                            }),
                        }, cx))?;
                        runner.frame(cx)?;
                        runner.update(cx, |view, window, _| {
                            assert_eq!(view.chats[session].scroller.item_count(), 0);
                            assert_eq!(window.shot_element_bounds(&draft).unwrap().size, size);
                        })?;
                        runner.click(cx, ClickTarget::Element { id: draft })?;
                        runner.step(Step::WaitElement { id: "chat-attachment-full-image".into() }, cx).await?;
                        runner.step(Step::Key { key: "escape".into() }, cx).await?;
                        runner.update(cx, |view, window, cx| {
                            view.threads_loaded = true;
                            view.send_prompt(window, cx);
                            view.on_event(ServerEvent::UserPrompt {
                                session: session.into(), item: "prompt".into(), text: String::new(),
                                paths: vec!["inbox/screenshot.png".into()], to_worker: false,
                            }, cx);
                        })?;
                        runner.frame(cx)?;
                        runner.update(cx, |view, window, _| {
                            assert_eq!(view.chats[session].scroller.item_count(), 1);
                            assert!(window.shot_element_bounds(&format!("chat-draft-image-{id}")).is_none());
                            let thumbnail = window.shot_element_bounds("chat-message-image-0-0").unwrap();
                            let bubble = window.shot_element_bounds("chat-message-bubble-0").unwrap();
                            assert_eq!(thumbnail.size, size);
                            assert!(bubble.contains(&thumbnail.center()));
                        })?;
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn folder_drop_preview_requires_confirmation_and_can_cancel() {
        use fuzukue_proto::FolderAttachmentRequest as Request;
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            let session = "folder-attachment-fixture";
            view.update(cx, |view, cx| {
                view.switch_session(session.into(), cx);
                view.threads_loaded = true;
                view.out = out.into();
                view.on_event(ServerEvent::ChatCapabilities {
                    effort_levels: vec![], mail_restricted: false, mcp_tools: vec![], folder_attachments: true,
                }, cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(),
                    read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let directory = tempfile::tempdir()?;
                    let folder = directory.path().join("synthetic-archive");
                    std::fs::create_dir_all(folder.join(".git"))?;
                    std::fs::write(folder.join(".git/ignored"), b"ignored")?;
                    std::fs::write(folder.join("notes.txt"), b"abc")?;
                    let id = runner.update(cx, |view, _, cx| {
                        view.add_attachments(session.into(), vec![folder], cx);
                        view.shot_last_attachment_id()
                    })?;
                    let copy = format!("copy-folder-{id}");
                    runner.step(Step::WaitElement { id: copy.clone() }, cx).await?;
                    ensure!(!messages.try_iter().any(|msg| matches!(msg, ClientMsg::FolderAttachment { .. })),
                        "the preview sent a request before confirmation");
                    runner.click(cx, ClickTarget::Element { id: copy })?;
                    let start = Instant::now();
                    loop {
                        runner.frame(cx)?;
                        if let Some(request) = messages.try_iter().find_map(|msg| match msg {
                            ClientMsg::FolderAttachment { id: found, request } if found == id => Some(request),
                            _ => None,
                        }) {
                            ensure!(matches!(request, Request::Begin { bytes: 3, entries: 1, .. }),
                                "the copy did not match the preview");
                            break;
                        }
                        ensure!(start.elapsed() < Duration::from_millis(TIMEOUT_MS), "copy did not start");
                        pause(cx, 30).await;
                    }
                    runner.click(cx, ClickTarget::Element { id: format!("remove-attachment-{id}") })?;
                    ensure!(messages.try_iter().any(|msg| matches!(msg, ClientMsg::CancelAttachment { id: found } if found == id)),
                        "removing the chip did not cancel the copy");
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::AttachmentProgress { id, offset: 0 }, cx);
                    })?;
                    ensure!(!messages.try_iter().any(|msg| matches!(msg, ClientMsg::FolderAttachment { .. })),
                        "the canceled copy accepted a late acknowledgement");
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn goal_tooltip_wraps_and_clears_the_composer() {
        let scenario: Scenario = serde_json::from_str(include_str!("../../../tests/shots/tooltip.json")).unwrap();
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench_at_size(cx, 600., 470.).unwrap();
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(),
                    read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    for step in scenario.steps {
                        match step {
                            Step::Size { .. } => {},
                            Step::Screenshot { .. } => runner.update(cx, |_, window, _| -> Result<()> {
                                let bounds = |id: &str| window.shot_element_bounds(id)
                                    .with_context(|| format!("element is not visible: {id}"));
                                let tooltip = bounds("tooltip-popup")?;
                                let status = bounds("chat-tail-activity-label")?;
                                let composer = bounds("composer-input-frame")?;
                                let viewport = window.viewport_size();
                                let rem = window.rem_size();
                                ensure!(tooltip.left() >= px(0.) && tooltip.right() <= viewport.width
                                        && tooltip.top() >= px(0.) && tooltip.bottom() <= status.top()
                                        && tooltip.bottom() < composer.top(),
                                    "tooltip must fit inside the window above the status: {tooltip:?}, {status:?}");
                                ensure!(tooltip.size.width <= rem * 24.
                                        && tooltip.size.height >= rem * 4. && tooltip.size.height <= rem * 12.,
                                    "tooltip should wrap into a bounded number of lines: {tooltip:?}");
                                Ok(())
                            })??,
                            step => runner.step(step, cx).await?,
                        }
                    }
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn composer_status_fits_and_aligns_with_stop() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                cx.set_reduce_motion(true);
                cx.spawn(async move |cx| {
                    let outcome: Result<()> = async {
                        for width in [600, 1440] {
                            let (handle, view) = cx.update(|cx| open_workbench_at_size(cx, width as f32, 900.))?;
                            let mut runner = Runner { handle, view, target: None, output: PathBuf::new(),
                                read_only: false, seen: HashMap::new() };
                            runner.step(Step::Locale { locale: "ja".into() }, cx).await?;
                            runner.step(Step::SeedComposerContext, cx).await?;
                            runner.frame(cx)?;
                            runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                            runner.step(Step::WaitElement { id: "chat-tail-activity-job-model-0".into() }, cx).await?;
                            runner.update(cx, |_, window, _| {
                                let chip = window.shot_element_bounds("chat-tail-activity-job-model-0").unwrap();
                                assert!(chip.left() >= px(0.) && chip.right() <= window.viewport_size().width);
                            })?;
                            runner.key(cx, "Escape")?;
                            for theme in [ShotTheme::Dark, ShotTheme::Light] {
                                runner.step(Step::Theme { theme }, cx).await?;
                                runner.step(Step::Hover { id: "chat-input".into() }, cx).await?;
                                runner.step(Step::Hover { id: "composer-context-ring".into() }, cx).await?;
                                runner.step(Step::WaitElement { id: "composer-context-tooltip".into() }, cx).await?;
                                runner.update(cx, |_, window, _| -> Result<()> {
                                    let viewport = window.viewport_size();
                                    ensure!(viewport.width == px(width as f32), "expected {width}px window");
                                    let bounds = |id: &str| window.shot_element_bounds(id)
                                        .with_context(|| format!("element is not visible at {width}px: {id}"));
                                    let ring = bounds("context-progress")?;
                                    let button = bounds("composer-context-ring")?;
                                    let stop = bounds("stop-prompt")?;
                                    let permission = bounds("permission-picker")?;
                                    let worker = bounds("worker-approval-picker")?;
                                    let composer = bounds("composer-input-frame")?;
                                    let tooltip = bounds("composer-context-tooltip")?;
                                    ensure!((button.center().x - stop.center().x).abs() <= px(1.)
                                            && (ring.center().x - button.center().x).abs() <= px(1.)
                                            && (ring.center().y - button.center().y).abs() <= px(1.),
                                        "context ring must be centered in its circular target and aligned under Stop");
                                    ensure!(composer.left() <= permission.left()
                                            && permission.right() <= worker.left() && worker.right() <= button.left()
                                            && (permission.center().y - button.center().y).abs() <= px(1.)
                                            && (worker.center().y - button.center().y).abs() <= px(1.),
                                        "composer status controls must fit side by side at {width}px");
                                    ensure!(tooltip.bottom() < composer.top() && tooltip.right() <= stop.right()
                                            && tooltip.top() >= px(0.) && tooltip.left() >= px(0.),
                                        "tooltip must clear the composer and stay within the viewport: {tooltip:?}, {composer:?}");
                                    Ok(())
                                })??;
                            }
                        }
                        Ok(())
                    }.await;
                    result.send(outcome).unwrap();
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn composer_sends_typed_escalation_and_ordinary_prompts() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                let session = String::from("escalation-fixture");
                view.update(cx, |view, cx| {
                    view.switch_session(session.clone(), cx);
                    view.threads_loaded = true;
                    view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                    view.out = out.into();
                });
                cx.spawn(async move |cx| {
                    let outcome: Result<()> = async {
                        let runner = Runner { handle, view, target: None, output: PathBuf::new(),
                            read_only: false, seen: HashMap::new() };
                        let click = |id: &str| ClickTarget::Element { id: id.into() };
                        let expect_prompt = |expected: &str| -> Result<()> {
                            let message = messages.try_recv()?;
                            ensure!(matches!(message, ClientMsg::Prompt { session: target, text }
                                if target == session && text == expected), "expected prompt {expected:?}");
                            ensure!(matches!(messages.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
                                "unexpected extra message");
                            Ok(())
                        };
                        runner.frame(cx)?;
                        runner.click(cx, click("chat-input"))?;
                        runner.type_text(cx, "Ordinary reply")?;
                        runner.frame(cx)?;
                        runner.click(cx, click("send-prompt"))?;
                        expect_prompt("Ordinary reply")?;
                        runner.click(cx, click("chat-input"))?;
                        runner.type_text(cx, "/escalate Manual reply")?;
                        runner.key(cx, "Ctrl+Enter")?;
                        expect_prompt("/escalate Manual reply")?;
                        Ok(())
                    }.await;
                    result.send(outcome).unwrap();
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn composer_layout_and_controls_follow_the_live_turn() {
        use fuzukue_proto::Actor;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                cx.set_reduce_motion(true);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                view.update(cx, |view, cx| {
                    view.switch_session("thread-1".into(), cx);
                    view.on_event(ServerEvent::ChatCapabilities {
                        effort_levels: vec!["high".into()], mail_restricted: false, mcp_tools: vec![], folder_attachments: false,
                    }, cx);
                });
                let (out, messages) = std::sync::mpsc::channel();
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        let session = String::from("thread-1");
                        runner.frame(cx)?;
                        let check_centers = |window: &Window| -> Result<Pixels> {
                            let bounds = |id: &str| {
                                window.shot_element_bounds(id)
                                    .with_context(|| format!("element is not visible: {id}"))
                            };
                            let input = bounds("chat-input")?;
                            let attach = bounds("attach-file")?;
                            ensure!((input.left() - attach.right()).abs() < px(1.),
                                "the composer input must sit directly beside the attachment button");
                            ensure!(attach.size.width == attach.size.height, "the attachment target must remain square");
                            for id in ["attach-file", "send-prompt"] {
                                let button = bounds(id)?;
                                ensure!((button.center().y - input.center().y).abs() < px(1.), "{id} is off-center");
                                ensure!((button.size.height - input.size.height).abs() < px(1.), "{id} and the single-line input differ in height");
                            }
                            let ring = bounds("composer-context-ring")?;
                            for id in ["permission-picker"] {
                                ensure!((bounds(id)?.center().y - ring.center().y).abs() < px(1.), "{id} is off-center");
                            }
                            Ok(input.size.height)
                        };
                        runner.update(cx, |view, _, cx| {
                            view.threads_loaded = true;
                            view.out = out.into();
                            cx.notify();
                        })?;
                        runner.frame(cx)?;
                        let click = |id: &str| ClickTarget::Element { id: id.into() };
                        let input_bounds = |runner: &Runner, cx: &mut AsyncApp| {
                            runner.update(cx, |_, window, _| window.shot_element_bounds("chat-input").unwrap())
                        };
                        let idle_bounds = input_bounds(&runner, cx)?;
                        runner.click(cx, click("send-prompt"))?;
                        runner.click(cx, click("chat-input"))?;
                        runner.step(Step::Key { key: "Ctrl+Enter".into() }, cx).await?;
                        assert!(messages.try_recv().is_err());

                        runner.step(Step::Type { text: "最初の入力".into() }, cx).await?;
                        runner.frame(cx)?;
                        runner.click(cx, click("send-prompt"))?;
                        assert!(matches!(messages.try_recv()?, ClientMsg::Prompt { session: target, text }
                            if target == session && text == "最初の入力"));
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::TurnActivity { session: session.clone(), running: true }, cx,
                        ))?;
                        runner.frame(cx)?;
                        assert_eq!(input_bounds(&runner, cx)?, idle_bounds);
                        assert!(runner.update(cx, |_, window, _|
                            window.shot_element_bounds("send-prompt").is_none())?);
                        runner.click(cx, click("stop-prompt"))?;
                        assert!(matches!(messages.try_recv()?, ClientMsg::Interrupt { session: target }
                            if target == session));

                        runner.click(cx, click("chat-input"))?;
                        runner.step(Step::Type { text: "追加の入力".into() }, cx).await?;
                        runner.frame(cx)?;
                        assert_eq!(input_bounds(&runner, cx)?, idle_bounds);
                        assert!(runner.update(cx, |_, window, _|
                            window.shot_element_bounds("stop-prompt").is_none())?);
                        runner.click(cx, click("send-prompt"))?;
                        assert!(matches!(messages.try_recv()?, ClientMsg::Prompt { session: target, text }
                            if target == session && text == "追加の入力"));
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::TurnActivity { session: session.clone(), running: false }, cx,
                        ))?;
                        runner.frame(cx)?;
                        let empty_height = runner.update(cx, |_, window, _| check_centers(window))??;
                        runner.click(cx, ClickTarget::Element { id: "chat-input".into() })?;
                        runner.step(Step::Type { text: "一行目".into() }, cx).await?;
                        runner.frame(cx)?;
                        assert_eq!(runner.update(cx, |_, window, _| check_centers(window))??, empty_height);
                        runner.step(Step::Type { text: "\n二行目\n三行目".into() }, cx).await?;
                        runner.wait_for(cx, "複数行の入力欄", TIMEOUT_MS, |_, window| {
                            let rem = window.rem_size();
                            let line = crate::appearance::UI_LINE_HEIGHT.to_pixels(rem.into(), rem);
                            window.shot_element_bounds("chat-input").is_some_and(|input| {
                                (input.size.height - empty_height - line * 2.).abs() < px(1.)
                                    && ["attach-file", "send-prompt"].into_iter().all(|id| {
                                        window.shot_element_bounds(id)
                                            .is_some_and(|button| (button.bottom() - input.bottom()).abs() < px(1.))
                                    })
                            })
                        }).await?;
                        for (event, status) in [
                            (ServerEvent::TurnActivity { session: session.clone(), running: true },
                                "考えている…"),
                            (ServerEvent::ToolCall {
                                session: session.clone(), item: "bash".into(), actor: Actor::Claude,
                                name: "Bash".into(), input: serde_json::json!({"command": "pwd"}),
                            }, "Bash を実行中…"),
                            (ServerEvent::ToolCall {
                                session: session.clone(), item: "read".into(), actor: Actor::Claude,
                                name: "Read".into(), input: serde_json::json!({"file_path": "README.md"}),
                            }, "Bash を実行中… +1"),
                            (ServerEvent::ApprovalRequested {
                                session: session.clone(), request_id: "approval".into(), actor: Actor::Claude,
                                summary: "Bash: pwd".into(), localized: None, shell_directory: None, shell_prefix: None,
                            }, "待機中…"),
                            (ServerEvent::ApprovalResolved {
                                request_id: "approval".into(), decision: "accept".into(), automatic: false,
                            }, "Bash を実行中… +1"),
                            (ServerEvent::ToolResult {
                                session: session.clone(), item: "bash".into(), ok: true, output: String::new(),
                            }, "Read を実行中…"),
                            (ServerEvent::ToolResult {
                                session: session.clone(), item: "read".into(), ok: true, output: String::new(),
                            }, "考えている…"),
                            (ServerEvent::TextDelta {
                                session: session.clone(), item: "reply".into(), actor: Actor::Claude,
                                text: "**原稿を確認した。**".into(),
                            }, "書いている…"),
                        ] {
                            runner.update(cx, |view, _, cx| view.on_event(event, cx))?;
                            runner.wait_for(cx, "activity at the chat tail only", TIMEOUT_MS, |view, window| {
                                view.chats[&view.active].turn_status().as_deref() == Some(status)
                                    && window.shot_element_bounds("send-prompt").is_some()
                                    && window.shot_element_bounds("stop-prompt").is_none()
                                    && view.chat_activity_clock.is_some()
                                    && window.shot_element_bounds("chat-tail-activity-icon")
                                        .zip(window.shot_element_bounds("chat-tail-activity-label"))
                                        .is_some_and(|(icon, label)| icon.size.width == px(14.) && icon.size.height == px(14.)
                                            && (icon.center().y - label.center().y).abs() <= px(0.5))
                            }).await?;
                        }
                        runner.update(cx, |view, _, cx| {
                            for index in 0..48 {
                                view.on_event(ServerEvent::TextDelta {
                                    session: session.clone(), item: format!("user:{index}"), actor: Actor::User,
                                    text: format!("以前の発言 {index}"),
                                }, cx);
                            }
                        })?;
                        runner.wait_for(cx, "activity after the last message", TIMEOUT_MS, |view, window| {
                            let last_message = view.chats[&view.active].scroller.item_count() - 1;
                            window.shot_element_bounds(&format!("chat-message-bubble-{last_message}"))
                                .zip(window.shot_element_bounds("chat-tail-activity"))
                                .is_some_and(|(message, activity)| message.bottom() <= activity.top())
                        }).await?;
                        let geometry = |runner: &Runner, cx: &mut AsyncApp| {
                            runner.update(cx, |view, window, _| {
                                let scroll = &view.chats[&view.active].scroller;
                                let viewport = scroll.viewport_bounds();
                                let strip = window.shot_element_bounds("chat-status-strip").unwrap();
                                assert!((viewport.bottom() - strip.top()).abs() < px(0.001));
                                for length in [viewport.size.height, strip.top(), strip.size.height] {
                                    let physical = f32::from(length) * window.scale_factor();
                                    assert!((physical - physical.round()).abs() < 0.001);
                                }
                                (
                                    scroll.item_count(),
                                    scroll.max_offset_for_scrollbar(),
                                    scroll.scroll_px_offset_for_scrollbar(),
                                    scroll.is_following_tail(),
                                    strip,
                                    viewport,
                                )
                            })
                        };
                        runner.step(Step::Scroll { pane: Pane::Chat, pixels: -8. }, cx).await?;
                        runner.wait_for(cx, "status and jump button while slightly above the bottom", TIMEOUT_MS, |view, window| {
                            let scroll = &view.chats[&view.active].scroller;
                            let last = format!("chat-message-bubble-{}", scroll.item_count() - 1);
                            !scroll.is_following_tail()
                                && window.shot_element_bounds("chat-tail-activity-label").is_some()
                                && window.shot_element_bounds("jump-to-latest")
                                    .zip(window.shot_element_bounds("chat-status-strip"))
                                    .zip(window.shot_element_bounds(&last))
                                    .is_some_and(|((jump, strip), message)| {
                                        (strip.top() - jump.bottom() - px(8.)).abs() < px(0.001)
                                            && (jump.center().x - strip.center().x).abs() < px(1.)
                                            && message.bottom() <= jump.top()
                                    })
                        }).await?;
                        runner.step(Step::Scroll { pane: Pane::Chat, pixels: -500. }, cx).await?;
                        runner.frame(cx)?;
                        let active = geometry(&runner, cx)?;
                        assert!(active.1.y > px(500.));
                        assert!(!active.3);
                        let status_width = runner.update(cx, |_, window, _|
                            window.shot_element_bounds("chat-tail-activity-metadata").unwrap().size.width)?;
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::TurnEnded { session: session.clone(), actor: Actor::Claude },
                            cx,
                        ))?;
                        runner.frame(cx)?;
                        assert_eq!(geometry(&runner, cx)?, active);
                        assert!(runner.update(cx, |view, window, _|
                            view.chat_activity_clock.is_none()
                                && window.shot_element_bounds("chat-tail-activity").is_none()
                                && window.shot_element_bounds("stop-prompt").is_none()
                                && window.shot_element_bounds("send-prompt").is_some())?);

                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::ChatWaiting { waiting: fuzukue_proto::ChatWaiting {
                                workspace: Some(fuzukue_proto::ChatWaitReason::BackgroundTest),
                                threads: Default::default(),
                                goal_status: Default::default(),
                            }},
                            cx,
                        ))?;
                        runner.frame(cx)?;
                        assert_eq!(geometry(&runner, cx)?, active);
                        assert_eq!(runner.update(cx, |_, window, _|
                            window.shot_element_bounds("chat-tail-activity-metadata").unwrap().size.width)?,
                            status_width);
                        runner.click(cx, ClickTarget::Element { id: "jump-to-latest".into() })?;
                        runner.wait_for(cx, "following the latest message", TIMEOUT_MS, |view, window| {
                            view.chats[&view.active].scroller.is_following_tail()
                                && window.shot_element_bounds("chat-tail-activity-label").is_some()
                                && window.shot_element_bounds("jump-to-latest").is_none()
                        }).await?;
                        let following = geometry(&runner, cx)?;
                        let commands = [
                            fuzukue_proto::BackgroundCommand {
                                id: "test".into(), summary: "cargo test -q".into(), started_at_ms: 1_000,
                                state: fuzukue_proto::BackgroundCommandState::Running,
                            },
                            fuzukue_proto::BackgroundCommand {
                                id: "monitor".into(), summary: "tail -f …".into(), started_at_ms: 30_000,
                                state: fuzukue_proto::BackgroundCommandState::Running,
                            },
                        ];
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::BackgroundCommands { threads: [(session.clone(), commands.to_vec())].into() },
                            cx,
                        ))?;
                        runner.frame(cx)?;
                        assert_eq!(geometry(&runner, cx)?, following);
                        runner.update(cx, |view, window, _| {
                            assert_eq!(view.background_command_activity(&session, 66_000),
                                Some(("コマンド実行中 2 件".into(), "cargo test -q", "1:05".into())));
                            let bounds = |id| window.shot_element_bounds(id).unwrap();
                            let phase = bounds("chat-tail-activity-label");
                            let count = bounds("chat-tail-activity-command-label");
                            let elapsed = bounds("chat-tail-activity-command-time");
                            assert!(count.left() > phase.right() && elapsed.left() > count.right());
                            assert!((count.center().y - phase.center().y).abs() <= px(0.5));
                            assert!(window.shot_element_bounds("chat-tail-activity-icon").is_some());
                            assert!(window.shot_element_bounds("send-prompt").is_some());
                            assert!(window.shot_element_bounds("stop-prompt").is_none());
                            assert!(view.chat_activity_clock.is_some());
                        })?;
                        runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                        runner.step(Step::WaitElement { id: "chat-tail-activity-command-1".into() }, cx).await?;
                        runner.step(Step::Key { key: "escape".into() }, cx).await?;
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::ChatWaiting { waiting: Default::default() }, cx,
                        ))?;
                        runner.frame(cx)?;
                        assert!(runner.update(cx, |view, window, _| {
                            view.chat_activity_clock.is_some()
                                && window.shot_element_bounds("chat-tail-activity-command-label").is_some()
                                && window.shot_element_bounds("chat-tail-activity-label").is_none()
                        })?);
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::BackgroundCommands { threads: Default::default() }, cx,
                        ))?;
                        runner.frame(cx)?;
                        assert!(runner.update(cx, |view, window, _| {
                            view.chat_activity_clock.is_none()
                                && window.shot_element_bounds("chat-tail-activity").is_none()
                        })?);
                        for goal_status in [fuzukue_proto::GoalTail::PendingVerification {
                            reason: "Check the real device".into(),
                        }, fuzukue_proto::GoalTail::Paused] {
                            runner.update(cx, |view, _, cx| view.on_event(
                                ServerEvent::ChatWaiting { waiting: fuzukue_proto::ChatWaiting {
                                    goal_status: [(session.clone(), goal_status)].into(),
                                    ..Default::default()
                                } }, cx,
                            ))?;
                            runner.frame(cx)?;
                            assert_eq!(geometry(&runner, cx)?, following);
                            assert!(runner.update(cx, |view, window, _|
                                view.chat_activity_clock.is_none()
                                    && window.shot_element_bounds("chat-tail-activity-label").is_some()
                                    && window.shot_element_bounds("chat-tail-activity-icon").is_none()
                                    && window.shot_element_bounds("chat-tail-activity-metadata").is_none())?);
                        }
                        runner.update(cx, |view, _, cx| view.on_event(
                            ServerEvent::ChatWaiting { waiting: Default::default() }, cx,
                        ))?;
                        runner.frame(cx)?;
                        assert_eq!(geometry(&runner, cx)?, following);
                        let details = || Step::WaitElement { id: "composer-context-details".into() };
                        runner.step(Step::Hover { id: "composer-context-ring".into() }, cx).await?;
                        runner.step(Step::WaitElement { id: "composer-context-tooltip".into() }, cx).await?;
                        runner.click(cx, ClickTarget::Element { id: "composer-context-ring".into() })?;
                        runner.step(Step::Hover { id: "chat-input".into() }, cx).await?;
                        runner.step(details(), cx).await?;
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn named_click_and_unicode_key_steps_send_the_real_chat_prompt() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                view.update(cx, |view, cx| {
                    view.out = out.into();
                    view.on_event(
                        ServerEvent::Threads {
                            threads: vec![ThreadView {
                            github: Default::default(),
                            untitled: false,
                            auto_attached: Vec::new(), suggestions: Vec::new(),
                                id: "thread-1".into(),
                                title: "煙テスト".into(),
                                updated: 0,
                                archived: false, pinned: false,
                                attached: vec![],
                                math_macros: vec![],
                                claude_session: None,
                                artifacts: vec![],
                                partner_settings: None, agent_overrides: Default::default(),
                    worker_settings: None,
                            }],
                        },
                        cx,
                    );
                });
                let scenario = serde_json::from_str(
                    r#"{"steps":[
                    {"action":"click","id":"chat-input"},
                    {"action":"type","text":"**太字** と *斜体* と `code`\n二行目"},
                    {"action":"assert_composer_decorations","styled":3,"dimmed":6},
                    {"action":"key","key":"Up"},
                    {"action":"assert_composer_decorations","styled":3,"dimmed":2},
                    {"action":"key","key":"Ctrl+Z"},
                    {"action":"assert_composer_decorations","styled":0,"dimmed":0},
                    {"action":"key","key":"Ctrl+Y"},
                    {"action":"assert_composer_decorations","styled":3,"dimmed":6},
                    {"action":"key","key":"Ctrl+Enter"}
                ]}"#,
                )
                .unwrap();
                cx.spawn(async move |cx| {
                    let runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    let outcome = runner.run(scenario, cx).await;
                    let _ = result.send((outcome, messages.try_recv()));
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
        let (outcome, message) = completed.recv().unwrap();
        outcome.unwrap();
        assert!(matches!(
            message.unwrap(),
            ClientMsg::Prompt { session, text } if session == "thread-1" && text == "**太字** と *斜体* と `code`\n二行目"
        ));
    }
    #[test]
    fn agent_settings_controls_and_history_jump_use_the_workbench() {
        use fuzukue_proto::{Actor, AgentCaller, AgentInfo, AgentInvocation, AgentOutcome, AgentSelection};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let settings = tempfile::tempdir()?;
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.appearance.isolate_shot_settings(settings.path().join("settings.toml"));
                        view.protocol = fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL;
                        let threads = [("target", "記録のあるスレッド"), ("other", "別のスレッド")]
                            .into_iter().map(|(id, title)| ThreadView {
                                github: Default::default(),
                                untitled: false,
                                id: id.into(), title: title.into(), updated: 0,
                                archived: false, pinned: false, attached: vec![], auto_attached: vec![],
                                suggestions: vec![], math_macros: vec![], artifacts: vec![], claude_session: None,
                                partner_settings: None, worker_settings: None, agent_overrides: Default::default(),
                            }).collect();
                        view.on_event(ServerEvent::Threads { threads }, cx);
                        view.select_thread("other".into(), cx);
                        let agent: AgentInfo = serde_json::from_value(serde_json::json!({
                            "name": "book", "description": "章構成を確認する",
                            "backend": "claude", "model": "claude-sonnet-5-5", "when": "章立てを見直すとき",
                        })).unwrap();
                        view.on_event(ServerEvent::Agents { agents: vec![agent] }, cx);
                        view.on_event(ServerEvent::AgentStats { stats: [("book".into(), Default::default())].into(),
                            never_used: vec!["book".into()] }, cx);
                        view.on_event(ServerEvent::ToolCall {
                            session: "target".into(), item: "delegate:history-test".into(), actor: Actor::System,
                            name: "delegate".into(), input: serde_json::json!({
                                "agent": "book", "model": "claude-sonnet-5-5", "started_at_ms": 1,
                            }),
                        }, cx);
                        view.on_event(ServerEvent::AgentHistory { agent: None, session: None, entries: vec![AgentInvocation {
                            started_at_ms: 1, workspace: "workspace".into(), session: "target".into(),
                            caller: Some(AgentCaller::FrontDesk), agent: "book".into(), selection: Some(AgentSelection::skill_default()),
                            duration_ms: 1000, outcome: AgentOutcome::Ok, tokens: Some(80),
                            job: "delegate:history-test".into(), title: "章立ての確認".into(),
                            routing: None, attempts: Vec::new(),
                        }] }, cx);
                    })?;
                    let steps: Scenario = serde_json::from_str(r#"{"steps":[
                        {"action":"click","id":"menu-help"},
                        {"action":"click","id":"tool-settings"},
                        {"action":"click","id":"settings-agents"},
                        {"action":"click","id":"agent-enabled-book"},
                        {"action":"click","id":"agent-caller-book-受付"},
                        {"action":"click","id":"agent-save-when-book"},
                        {"action":"click","id":"settings-agent-history"},
                        {"action":"click","id":"agent-history-entry-0"},
                        {"action":"assert_active_thread","title":"記録のあるスレッド"},
                        {"action":"wait_element","id":"delegate-block-delegate:history-test"},
                        {"action":"open_delegate_details"},
                        {"action":"wait_element","id":"delegate-details-delegate:history-test-content"}
                    ]}"#)?;
                    for step in steps.steps {
                        runner.step(step, cx).await?;
                    }
                    let requests: Vec<_> = messages.try_iter().collect();
                    assert!(requests.iter().any(|message| matches!(message, ClientMsg::SetAgentPolicy {
                        name, policy: Some(policy)
                    } if name == "book" && policy.enabled == Some(false))));
                    assert!(requests.iter().any(|message| matches!(message, ClientMsg::SetAgentCaller {
                        name, caller: AgentCaller::FrontDesk, allowed: false
                    } if name == "book")));
                    assert!(requests.iter().any(|message| matches!(message, ClientMsg::SetAgentPolicy {
                        name, policy: Some(policy)
                    } if name == "book" && policy.when.as_deref() == Some("章立てを見直すとき"))));
                    assert!(requests.iter().any(|message| matches!(message, ClientMsg::GetAgentHistory { .. })));
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn close_controls_and_escape_close_the_top_view_and_guard_deletion() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            let draft = fuzukue_proto::CommentView {
                card: "intro".into(), text: "Draft comment".into(), quote: None,
                proof: None, source: None, created: 1,
            };
            view.update(cx, |view, _| {
                view.out = out.into();
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.status = crate::ConnectionStatus::Connected;
                view.comments = vec![draft.clone()];
                view.right = crate::RightPane::DeckComments;
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let escape = || Step::Key { key: "escape".into() };
                    let wait = |id: &str| Step::WaitElement { id: id.into() };

                    runner.step(click("rm-comment-0"), cx).await?;
                    runner.step(wait("trash-confirmation"), cx).await?;
                    assert!(!messages.try_iter().any(|request| matches!(request, ClientMsg::Trash { .. })));
                    runner.step(escape(), cx).await?;
                    runner.wait_for(cx, "confirmation closed", TIMEOUT_MS, |_, window| {
                        window.shot_element_bounds("trash-confirmation").is_none()
                    }).await?;
                    assert!(runner.update(cx, |view, _, _| view.right == crate::RightPane::DeckComments)?);
                    runner.step(click("rm-comment-0"), cx).await?;
                    runner.step(wait("trash-confirm"), cx).await?;
                    runner.step(click("trash-confirm"), cx).await?;
                    assert!(messages.try_iter().any(|request| matches!(request,
                        ClientMsg::Trash { action: fuzukue_proto::TrashAction::Delete {
                            target: fuzukue_proto::TrashTarget::Comment { draft: sent, .. }
                        } } if sent == draft
                    )));

                    runner.wait_for(cx, "confirmed dialog closed", TIMEOUT_MS, |_, window| {
                        window.shot_element_bounds("trash-confirmation").is_none()
                    }).await?;
                    runner.step(click("close-pane"), cx).await?;
                    assert!(runner.update(cx, |view, _, _| view.right == crate::RightPane::None)?);
                    runner.click_menu(cx, "menu-view", "tool-library")?;
                    runner.step(wait("library-close"), cx).await?;
                    runner.step(click("library-ledger-tab"), cx).await?;
                    assert!(runner.update(cx, |view, _, _| view.ledger_open())?);
                    runner.step(wait("ledger-close"), cx).await?;
                    runner.step(escape(), cx).await?;
                    runner.step(wait("library-close"), cx).await?;
                    runner.step(escape(), cx).await?;
                    assert!(runner.update(cx, |view, _, _| view.right == crate::RightPane::None)?);

                    runner.click_menu(cx, "menu-view", "tool-trash")?;
                    runner.step(wait("trash-close"), cx).await?;
                    runner.step(click("trash-close"), cx).await?;
                    assert!(runner.update(cx, |view, _, _| view.right == crate::RightPane::None)?);
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn worker_block_stays_below_its_reply_and_above_later_user_messages() {
        use fuzukue_proto::Actor;

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            let session = "thread-placement";
            let job = "delegate:proof";
            let emit = move |view: &mut Workbench, cx: &mut Context<Workbench>, item: &str, actor, text: &str| {
                view.on_event(ServerEvent::TextDelta {
                    session: session.into(), item: item.into(), actor, text: text.into(),
                }, cx);
            };
            view.update(cx, |view, cx| {
                view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                view.out = out.into();
                view.active = session.into();
                emit(view, cx, "user:1", Actor::User, "First question.");
                view.on_event(ServerEvent::ToolCall {
                    session: session.into(), item: job.into(), actor: Actor::System, name: "delegate".into(),
                    input: serde_json::json!({"agent": "proof", "model": "claude-opus-5-5", "started_at_ms": 1}),
                }, cx);
                emit(view, cx, "delegate:proof:work", Actor::Claude, "Work in progress.");
                emit(view, cx, "reply:1", Actor::Claude, "I am checking that.");
                view.on_event(ServerEvent::TurnEnded { session: session.into(), actor: Actor::Claude }, cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let assert_order = |above: &str, below: &str| Step::AssertOrder { above: above.into(), below: below.into() };
                    let block = "delegate-block-delegate:proof";
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    let worker_text = "chat-copy-2-worker-0";
                    let report = "delegate-report-delegate:proof";
                    let transcript = "delegate-text-delegate:proof";
                    let visible = |runner: &Runner, cx: &mut AsyncApp, id| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    runner.step(assert_order("chat-message-bubble-1", block), cx).await?;
                    assert!(!visible(&runner, cx, "chat-partner-reply")?);
                    runner.update(cx, |view, _, cx| {
                        emit(view, cx, "user:2", Actor::User, "A later question.");
                        emit(view, cx, "delegate:proof:work", Actor::Claude, " Still checking.");
                    })?;
                    runner.step(assert_order(block, "chat-copy-1"), cx).await?;
                    assert!(!visible(&runner, cx, worker_text)?);
                    runner.step(click(transcript), cx).await?;
                    runner.step(assert_order(worker_text, "chat-message-bubble-3"), cx).await?;
                    assert!(!visible(&runner, cx, "chat-copy-3")?);
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::ToolResult {
                            session: session.into(), item: job.into(), ok: true, output: r#"{"elapsed_ms":500,"text":"Done."}"#.into(),
                        }, cx);
                        emit(view, cx, "reply:2", Actor::Claude, "The check passed.");
                        view.on_event(ServerEvent::TurnEnded { session: session.into(), actor: Actor::Claude }, cx);
                    })?;
                    runner.step(assert_order("chat-copy-3", "chat-partner-reply"), cx).await?;
                    assert!(!visible(&runner, cx, report)?);
                    runner.step(click(transcript), cx).await?;
                    runner.step(Step::WaitElement { id: report.into() }, cx).await?;
                    runner.step(click(transcript), cx).await?;
                    runner.click_menu(cx, "menu-help", "tool-settings")?;
                    runner.step(click("show-worker-messages"), cx).await?;
                    runner.step(click("close-settings"), cx).await?;
                    runner.step(Step::WaitElement { id: report.into() }, cx).await?;
                    runner.click_menu(cx, "menu-help", "tool-settings")?;
                    runner.step(click("show-worker-messages"), cx).await?;
                    runner.step(click("close-settings"), cx).await?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, report)?);
                    runner.step(Step::Click { id: Some("chat-rewrite".into()), x: None, y: None }, cx).await?;
                    assert!(messages.try_iter().any(|request| matches!(request, ClientMsg::RewriteReply {
                        session: sent, item
                    } if sent == session && item == "reply:2")));
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::ReplySuperseded { session: session.into(), items: vec!["reply:2".into()] }, cx);
                    })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("chat-rewrite").is_none())?);
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn role_controls_choose_worker_and_route_the_real_composer() {
        use fuzukue_proto::{Actor, ChatSettings, DelegateSettings, CLAUDE_OPUS, PROTOCOL_VERSION};
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            cx.set_global(gpui_kit::ShotClipboard::default());
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            let session = "thread-roles";
            view.update(cx, |view, cx| {
                view.out = out.into();
                view.protocol = PROTOCOL_VERSION;
                view.on_event(ServerEvent::ChatCapabilities {
                    effort_levels: vec!["high".into()], mail_restricted: false, mcp_tools: vec![], folder_attachments: false,
                }, cx);
                view.on_event(ServerEvent::Threads { threads: vec![ThreadView {
                            github: Default::default(),
                            untitled: false,
                            auto_attached: Vec::new(), suggestions: Vec::new(),
                    id: session.into(), title: "役の動作確認".into(), updated: 0, archived: false, pinned: false,
                    attached: vec![], math_macros: vec![], claude_session: None, artifacts: vec![],
                    partner_settings: Some(ChatSettings {
                        model: Some("gpt-6.2-sol".into()), ..Default::default()
                    }),
                    agent_overrides: Default::default(),
                    worker_settings: Some(DelegateSettings::default()),
                }] }, cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let steps: Scenario = serde_json::from_str(r#"{"steps":[
                        {"action":"click","id":"thread-info-toggle"},
                        {"action":"click","id":"thread-worker-picker"},
                        {"action":"key","key":"down"},
                        {"action":"key","key":"enter"},
                        {"action":"click","id":"thread-info-toggle"}
                    ]}"#)?;
                    for step in steps.steps {
                        runner.step(step, cx).await?;
                    }
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::SetThreadWorker { session: id, settings }
                            if id == session && settings.model == CLAUDE_OPUS && settings.effort == "high"
                    )));
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::TurnActivity { session: session.into(), running: true }, cx);
                        view.on_event(ServerEvent::ToolCall {
                            session: session.into(), item: "delegate:test".into(), actor: Actor::System,
                            name: "delegate".into(), input: serde_json::json!({
                                "title": "R-81 Check the citations", "label": "Check citations", "card_id": "R-81",
                                "model": CLAUDE_OPUS, "started_at_ms": 1,
                                "selection": {"backend": "claude", "model": CLAUDE_OPUS, "effort": "high"},
                                "routing": {"tier": "heavy", "reason": "Escalated"}
                            }),
                        }, cx);
                        for (item, ok) in [("delegate:test:read", true), ("delegate:test:check", false)] {
                            view.on_event(ServerEvent::ToolCall {
                                session: session.into(), item: item.into(), actor: Actor::Claude,
                                name: "Read".into(), input: serde_json::json!({"file_path": "README.md"}),
                            }, cx);
                            view.on_event(ServerEvent::ToolResult {
                                session: session.into(), item: item.into(), ok, output: "Tool output".into(),
                            }, cx);
                        }
                        view.on_event(ServerEvent::TextDelta {
                            session: session.into(), item: "delegate:test:commentary".into(), actor: Actor::Claude,
                            text: "I checked the README.".into(),
                        }, cx);
                    })?;
                    let summary = "delegate-tools-delegate:test";
                    let tool = "chat-thread-roles-0-DelegateTool(0)";
                    runner.step(Step::Click { id: Some("delegate-text-delegate:test".into()), x: None, y: None }, cx).await?;
                    runner.step(Step::WaitElement { id: "chat-copy-0-worker-2".into() }, cx).await?;
                    let visible = |runner: &Runner, cx: &mut AsyncApp, id| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).is_some())
                    };
                    runner.step(Step::AssertOrder { above: "chat-worker-message-0-2".into(),
                        below: "chat-copy-0-worker-2".into() }, cx).await?;
                    runner.click(cx, ClickTarget::Element { id: "delegate-text-delegate:test".into() })?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "chat-worker-message-0-2")?);
                    let progress = "delegate-summary-delegate:test";
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::DelegateProgress {
                        session: session.into(), job: "delegate:test".into(), summary: " ".into(),
                    }, cx))?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, progress)?);
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::DelegateProgress {
                        session: session.into(), job: "delegate:test".into(), summary: "仮定を照合したよ〜".into(),
                    }, cx))?;
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::TextDelta {
                        session: session.into(), item: "delegate:test:commentary".into(), actor: Actor::Claude,
                        text: "\nI checked another reference.".repeat(10),
                    }, cx))?;
                    runner.step(Step::WaitElement { id: progress.into() }, cx).await?;
                    assert!(visible(&runner, cx, summary)?);
                    assert!(!visible(&runner, cx, tool)?);
                    assert!(visible(&runner, cx, "chat-tail-activity-label")?);
                    let assert_separator = |window: &Window, left: &str, dot: &str, right: &str| {
                        let left = window.shot_element_bounds(left).unwrap();
                        let dot = window.shot_element_bounds(dot).unwrap();
                        let right = window.shot_element_bounds(right).unwrap();
                        let left_gap = dot.left() - left.right();
                        let right_gap = right.left() - dot.right();
                        assert_eq!(dot.size, gpui_kit::size(px(3.), px(3.)));
                        assert!((left_gap - right_gap).abs() <= px(0.5), "{left_gap:?} vs {right_gap:?}");
                        assert!((left_gap - px(6.)).abs() <= px(0.5));
                    };
                    runner.update(cx, |_, window, _| assert_separator(
                        window, "chat-tail-activity-label", "chat-tail-activity-separator-metadata",
                        "chat-tail-activity-metadata",
                    ))?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-label-0")?);
                    runner.step(Step::Hover { id: "chat-tail-activity-trigger".into() }, cx).await?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-label-0".into() }, cx).await?;
                    runner.update(cx, |_, window, _| assert_separator(
                        window, "chat-tail-activity-job-title-0", "chat-tail-activity-job-separator-0",
                        "chat-tail-activity-job-time-0",
                    ))?;
                    assert!(runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("chat-tail-activity-job-icon-0")
                            .zip(window.shot_element_bounds("chat-tail-activity-job-label-0"))
                            .is_some_and(|(icon, label)| icon.size.width == px(14.) && icon.size.height == px(14.)
                                && (icon.center().y - label.center().y).abs() <= px(0.5))
                    })?);
                    assert!(visible(&runner, cx, "chat-tail-activity-job-error-0")?);
                    assert!(visible(&runner, cx, "chat-tail-activity-job-rename-0")?);
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-dismiss-0")?);
                    for part in ["card", "model", "effort", "tier", "backend", "last-tool"] {
                        let id = format!("chat-tail-activity-job-{part}-0");
                        assert!(runner.update(cx, |_, window, _| window.shot_element_bounds(&id).is_some())?);
                    }
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-tool-status-0")?);
                    runner.step(Step::Hover { id: "chat-tail-activity-job-details-0".into() }, cx).await?;
                    pause(cx, 350).await;
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "chat-tail-activity-job-details-0")?);
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-job-details-0".into() })?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-preview-0".into() }, cx).await?;
                    assert!(visible(&runner, cx, "chat-tail-activity-job-tool-status-0")?);
                    let (preview_height, line_height) = runner.update(cx, |_, window, _| {
                        let line = crate::appearance::UI_LINE_HEIGHT.to_pixels(gpui_kit::rems(0.875).into(), window.rem_size());
                        let preview = window.shot_element_bounds("chat-tail-activity-job-preview-0").unwrap();
                        (preview.size.height, line)
                    })?;
                    ensure!(preview_height >= line_height * 5. && preview_height < line_height * 7.,
                        "worker preview should be about six lines, measured {preview_height:?} with a line height of {line_height:?}");
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-job-full-0".into() })?;
                    runner.step(Step::WaitElement { id: "chat-worker-message-0-2".into() }, cx).await?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-details")?);
                    runner.step(Step::Hover { id: "chat-tail-activity-trigger".into() }, cx).await?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-label-0".into() }, cx).await?;
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-details")?);
                    runner.step(Step::Hover { id: "chat-input".into() }, cx).await?;
                    runner.step(Step::Hover { id: "chat-tail-activity-trigger".into() }, cx).await?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-label-0".into() }, cx).await?;
                    runner.step(Step::Hover { id: "chat-input".into() }, cx).await?;
                    runner.wait_for(cx, "work details to close when unhovered", TIMEOUT_MS, |_, window| {
                        window.shot_element_bounds("chat-tail-activity-job-label-0").is_none()
                    }).await?;
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                    runner.step(Step::WaitElement { id: "chat-role-target".into() }, cx).await?;
                    runner.step(Step::Hover { id: "chat-input".into() }, cx).await?;
                    pause(cx, 350).await;
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "chat-role-target")?);
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "chat-role-target")?);
                    let click_summary = || Step::Click { id: Some(summary.into()), x: None, y: None };
                    runner.step(click_summary(), cx).await?;
                    runner.step(Step::WaitElement { id: tool.into() }, cx).await?;
                    runner.step(click_summary(), cx).await?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, tool)?);
                    let steps: Scenario = serde_json::from_str(r#"{"steps":[
                        {"action":"click","id":"chat-input"},
                        {"action":"type","text":"相談を続ける。"},
                        {"action":"key","key":"Ctrl+Enter"},
                        {"action":"click","id":"chat-tail-activity-trigger"},
                        {"action":"wait_element","id":"chat-role-target"},
                        {"action":"click","id":"chat-role-target"},
                        {"action":"wait_element","id":"clear-worker-target"},
                        {"action":"click","id":"chat-input"},
                        {"action":"type","text":"証明中の参照だけ確認して。"},
                        {"action":"key","key":"Ctrl+Enter"}
                    ]}"#)?;
                    for step in steps.steps {
                        runner.step(step, cx).await?;
                    }
                    runner.update(cx, |view, _, cx| {
                        view.on_event(ServerEvent::TurnActivity { session: session.into(), running: false }, cx);
                    })?;
                    let steps: Scenario = serde_json::from_str(r#"{"steps":[
                        {"action":"click","id":"clear-worker-target"},
                        {"action":"click","id":"chat-input"},
                        {"action":"type","text":"受付に戻って相談する。"},
                        {"action":"click","id":"send-prompt"}
                    ]}"#)?;
                    for step in steps.steps {
                        runner.step(step, cx).await?;
                    }
                    let prompts: Vec<_> = messages.try_iter().filter_map(|message| match message {
                        ClientMsg::Prompt { session: id, text } if id == session => Some((text, None)),
                        ClientMsg::WorkerPrompt { session: id, text, job, .. } if id == session => Some((text, Some(job))),
                        _ => None,
                    }).collect();
                    assert_eq!(prompts, [
                        ("相談を続ける。".into(), None),
                        ("証明中の参照だけ確認して。".into(), Some("delegate:test".into())),
                        ("受付に戻って相談する。".into(), None),
                    ]);
                    runner.step(click_summary(), cx).await?;
                    runner.step(Step::WaitElement { id: tool.into() }, cx).await?;
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::ToolResult {
                        session: session.into(), item: "delegate:test".into(), ok: true,
                        output: serde_json::json!({"elapsed_ms": 2000, "text": "The check passed."}).to_string(),
                    }, cx))?;
                    runner.step(Step::AssertDelegateReport { model: CLAUDE_OPUS.into() }, cx).await?;
                    assert!(visible(&runner, cx, tool)?);
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-label-0")?);

                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-report-0".into() }, cx).await?;
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-job-copy-0".into() })?;
                    let copied = runner.update(cx, |_, _, cx| cx.read_from_clipboard().and_then(|value| value.text()))?;
                    assert_eq!(copied.as_deref(), Some("delegate:test"));
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-rename-0")?);
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-job-report-0".into() })?;
                    runner.step(Step::WaitElement { id: "job-report-dialog".into() }, cx).await?;
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-dismiss-0".into() }, cx).await?;
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-job-dismiss-0".into() })?;
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::DismissJob { session: id, job } if id == session && job == "delegate:test"
                    )));
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::DelegateDismissed {
                        session: session.into(), job: "delegate:test".into(),
                    }, cx))?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-trigger")?);
                    runner.step(Step::AssertDelegateReport { model: CLAUDE_OPUS.into() }, cx).await?;

                    runner.update(cx, |view, _, cx| {
                        for index in 0..30 {
                            view.on_event(ServerEvent::ToolCall {
                                session: session.into(), item: format!("delegate:overflow-{index}"),
                                actor: Actor::System, name: "delegate".into(),
                                input: serde_json::json!({"title": format!("Worker job {index}"), "model": CLAUDE_OPUS}),
                            }, cx);
                        }
                    })?;
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "chat-tail-activity-trigger".into() })?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-label-0".into() }, cx).await?;
                    assert!(runner.update(cx, |_, window, _| {
                        let panel = window.shot_element_bounds("chat-tail-activity-details").unwrap();
                        let strip = window.shot_element_bounds("chat-tail-activity-trigger").unwrap();
                        panel.size.height <= px(480.) && panel.top() >= px(0.) && panel.bottom() <= strip.top() + px(1.)
                    })?);
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-label-29")?);
                    let wheel = |cx: &mut AsyncApp, amount| runner.update(cx, |_, window, cx| {
                        use gpui_kit::{ScrollDelta, ScrollEvent};
                        let position = window.shot_element_bounds("chat-tail-activity-job-scroll").unwrap().center();
                        let delta = ScrollDelta::Pixels(point(px(0.), px(amount)));
                        window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                        window.dispatch_event(PlatformInput::Scroll(ScrollEvent { position, delta, ..Default::default() }), cx);
                    });
                    wheel(cx, -5_000.)?;
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "chat-tail-activity-job-label-29")?);
                    wheel(cx, 5_000.)?;
                    runner.frame(cx)?;
                    assert!(visible(&runner, cx, "chat-tail-activity-job-label-0")?);
                    for _ in 0..29 {
                        runner.key(cx, "down")?;
                        runner.frame(cx)?;
                    }
                    assert!(visible(&runner, cx, "chat-tail-activity-job-label-29")?);
                    runner.key(cx, "up")?;
                    runner.frame(cx)?;
                    runner.key(cx, "enter")?;
                    runner.step(Step::WaitElement { id: "chat-tail-activity-job-tool-status-28".into() }, cx).await?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-job-tool-status-29")?);
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(!visible(&runner, cx, "chat-tail-activity-details")?);
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    fn test_pdf(label: &str) -> Vec<u8> {
        test_pdf_pages(&[(label, 2100, 1840)])
    }

    fn test_pdf_pages(pages: &[(&str, u32, u32)]) -> Vec<u8> {
        let kids = (0..pages.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect::<Vec<_>>().join(" ");
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        for (index, &(label, height, baseline)) in pages.iter().enumerate() {
            let stream = 5 + 2 * index;
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 {height}] /Contents {stream} 0 R /Resources << /Font << /F1 3 0 R >> >> >>"));
            let content = format!("BT /F1 14 Tf 80 {baseline} Td ({label}) Tj ET");
            objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
        }
        let mut pdf = "%PDF-1.4\n".to_owned();
        let mut xref = "0000000000 65535 f \n".to_owned();
        for (index, body) in objects.iter().enumerate() {
            xref.push_str(&format!("{:010} 00000 n \n", pdf.len()));
            pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", index + 1));
        }
        let offset = pdf.len();
        let size = objects.len() + 1;
        pdf.push_str(&format!("xref\n0 {size}\n{xref}trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{offset}\n%%EOF\n"));
        pdf.into_bytes()
    }

    async fn check_paper_pdf(
        runner: &mut Runner,
        messages: &std::sync::mpsc::Receiver<ClientMsg>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        use gpui_kit::{ScrollDelta, ScrollEvent};
        use fuzukue_proto::PdfCardRegion;
        use sha2::{Digest, Sha256};

        let file = tempfile::Builder::new().suffix(".pdf").tempfile()?;
        let original = test_pdf("Original");
        let updated = test_pdf("Updated");
        std::fs::write(file.path(), &original)?;
        let path = "papers/test/build/paper.pdf".to_owned();
        let digest = format!("{:x}", Sha256::digest(&original));
        let map = vec![PdfCardRegion {
            page: 0, card: "lemma".into(), proof: false, rect: [0.12, 0.115, 0.4, 0.03],
        }];
        runner.update(cx, |view, _, cx| {
            let style = view.appearance.pdf_style.clone();
            let local = file.path().to_path_buf();
            let pdf = cx.new(|cx| gpui_pdf::PdfView::new(local,
                std::rc::Rc::new(move || style.get()), std::rc::Rc::new(|| 1.0), cx));
            pdf.update(cx, |pdf, cx| pdf.fit_width(cx));
            let source = path.clone();
            cx.subscribe(&pdf, move |this, pdf, event, cx| this.on_pdf_event(&source, &pdf, event, cx)).detach();
            view.pdf_path = Some(path.clone());
            view.pdf_digest = Some(digest.clone());
            view.pdf = Some((path.clone(), pdf));
            view.receive_paper_pdf_map(view.deck.clone(), path.clone(), digest, map.clone(), true, cx);
        })?;
        let geometry = |window: &Window| ["workbench-sidebar", "paper-centre-column", "paper-chat-column"]
            .map(|id| window.shot_element_bounds(id).expect("paper column bounds"));
        runner.frame(cx)?;
        let before = runner.update(cx, |_, window, _| geometry(window))?;
        runner.step(Step::Click { id: Some("paper-show-pdf".into()), x: None, y: None }, cx).await?;
        runner.step(Step::WaitPdf, cx).await?;
        assert_eq!(runner.update(cx, |_, window, _| geometry(window))?, before);

        let page_y = |runner: &Runner, cx: &mut AsyncApp| runner.update(cx, |view, _, cx| {
            view.pdf.as_ref().unwrap().1.read(cx).page_screen_bounds(0, (0., 0., 0., 0.)).unwrap().origin.y
        });
        let top = page_y(runner, cx)?;
        let wheel = runner.update(cx, |_, window, cx| {
            let position = window.shot_element_bounds("pdf-scroll").unwrap().center();
            let wheel = window.viewport_size().height * 0.9;
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
            window.dispatch_event(PlatformInput::Scroll(ScrollEvent {
                position, delta: ScrollDelta::Pixels(point(px(0.), -wheel)), ..Default::default()
            }), cx);
            wheel
        })?;
        runner.frame(cx)?;
        assert!((page_y(runner, cx)? - (top - wheel)).abs() < px(2.));
        let blank = runner.update(cx, |_, window, _| window.shot_element_bounds("pdf-scroll").unwrap().center())?;
        runner.click(cx, ClickTarget::Coordinates { x: blank.x.into(), y: blank.y.into() })?;
        runner.frame(cx)?;
        let transcript = runner.update(cx, |view, _, _| view.chats[&view.active].scroller.clone())?;
        let thread_position = (transcript.logical_scroll_top(), transcript.is_following_tail());
        for key in ["home", "pagedown", "space", "shift-space", "pageup", "down", "up", "end", "home"] {
            runner.step(Step::Key { key: key.into() }, cx).await?;
            runner.frame(cx)?;
            assert_eq!((transcript.logical_scroll_top(), transcript.is_following_tail()), thread_position, "PDF {key} moved the thread");
            let y = page_y(runner, cx)?;
            if matches!(key, "home" | "pageup" | "up") {
                assert!((y - top).abs() < px(2.), "{key}: {y:?} vs {top:?}");
            } else {
                assert!(y < top, "{key}: {y:?} vs {top:?}");
            }
        }

        runner.step(Step::ClickPdfCard { card: "lemma".into(), ctrl: false }, cx).await?;
        let popup = |runner: &Runner, cx: &mut AsyncApp| runner.update(cx, |_, window, _| {
            window.shot_element_bounds("pdf-card-popup").expect("visible popup")
        });
        let anchor = popup(runner, cx)?.origin;
        runner.step(Step::Click { id: Some("edit-pdf-card".into()), x: None, y: None }, cx).await?;
        runner.frame(cx)?;
        let editing = popup(runner, cx)?;
        assert_eq!(editing.origin, anchor);
        assert!(editing.size.height > px(300.));
        runner.step(Step::Key { key: "Ctrl+Enter".into() }, cx).await?;

        // Simulate the save and rebuild response without starting a server.
        let scrolled = page_y(runner, cx)?;
        runner.update(cx, |view, _, cx| {
            view.on_event(ServerEvent::CardSaveResult {
                request: view.card_save_request, outcome: fuzukue_proto::CardSaveOutcome::Saved,
            }, cx);
            assert!(view.card_editor("lemma", false).is_none());
            view.pdf.as_ref().unwrap().1.update(cx, |pdf, cx| pdf.replace_bytes(updated.clone(), cx));
        })?;
        runner.wait_for_with_context(cx, "replacement PDF", TIMEOUT_MS, |view, _, cx| {
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            pdf.loaded_bytes() == Some(updated.as_slice()) && pdf.page_ready(0)
        }).await?;
        assert_eq!(popup(runner, cx)?.origin, anchor);
        assert!((page_y(runner, cx)? - scrolled).abs() < px(2.));
        runner.step(Step::Click { id: Some("chat-input".into()), x: None, y: None }, cx).await?;
        runner.frame(cx)?;
        assert_eq!(popup(runner, cx)?.origin, anchor);
        let outside = runner.update(cx, |_, window, _| {
            let area = window.shot_element_bounds("pdf-scroll").unwrap();
            point(area.center().x, area.bottom() - px(12.))
        })?;
        runner.click(cx, ClickTarget::Coordinates { x: outside.x.into(), y: outside.y.into() })?;
        runner.wait_for(cx, "dismissed PDF card", TIMEOUT_MS, |view, _| view.paper_pdf.popup.is_none()).await?;
        let (from, to) = runner.update(cx, |view, _, cx| {
            view.receive_paper_pdf_map(view.deck.clone(), path.clone(), format!("{:x}", Sha256::digest(&updated)),
                map, true, cx);
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            let at = |x| pdf.page_screen_bounds(0, (x, 0.122, 0., 0.)).unwrap().origin;
            (at(0.132), at(0.23))
        })?;
        runner.drag(cx, from, to)?;
        runner.wait_for(cx, "PDF selection comment", TIMEOUT_MS, |view, _| view.paper_pdf.selection_comment.is_some()).await?;
        runner.frame(cx)?;
        let assert_comment_only = |view: &Workbench, window: &Window| {
            assert!(view.paper_pdf.popup.is_none());
            assert!(window.shot_element_bounds("pdf-card-popup").is_none());
            assert!(window.shot_element_bounds("open-pdf-card").is_none());
            assert!(window.shot_element_bounds("edit-pdf-card").is_none());
            assert!(window.shot_element_bounds("comment-pdf-selection").is_some());
        };
        runner.update(cx, |view, window, cx| {
            assert_comment_only(view, window);
            let image = window.shot_element_bounds("pdf-page-image-0").unwrap();
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            let page = pdf.page_screen_bounds(0, (0., 0., 1., 1.)).unwrap()
                .intersect(&window.shot_element_bounds("pdf-scroll").unwrap());
            assert!((image.origin.x - page.origin.x).abs() < px(0.6), "{image:?} vs {page:?}");
            assert!((image.origin.y - page.origin.y).abs() < px(0.6), "{image:?} vs {page:?}");
            assert!((image.size.width - page.size.width).abs() < px(1.1), "{image:?} vs {page:?}");
            assert!((image.size.height - page.size.height).abs() < px(1.1), "{image:?} vs {page:?}");
            let text = gpui_pdf::extract_page_text(&gpui_pdf::parse(updated.into()).unwrap(), 0).unwrap();
            let r = text.locate("Updated", 0)[0];
            let expected = pdf.page_screen_bounds(0, (r.x, r.y, r.w, r.h)).unwrap();
            let actual = window.shot_element_bounds("pdf-selection-0-0").unwrap();
            assert!((actual.origin.x - expected.origin.x).abs() < px(0.6));
            assert!((actual.origin.y - expected.origin.y).abs() < px(0.6));
            assert!((actual.size.width - expected.size.width).abs() < px(1.1));
            assert!((actual.size.height - expected.size.height).abs() < px(1.1));
        })?;
        runner.step(Step::Click { id: Some("comment-pdf-selection".into()), x: None, y: None }, cx).await?;
        runner.update(cx, |view, _, _| {
            let target = view.comment_target.as_ref().expect("PDF comment target");
            assert_eq!(target.card, "lemma");
            assert_eq!(target.quote.as_ref().unwrap().split_whitespace().collect::<String>(), "Updated");
            assert!(view.paper_pdf.popup.is_none());
        })?;

        // Consecutive clicks select a word or line. The initial single click must
        // never open the card preview when it turns into a multi-click gesture.
        for clicks in [2, 3] {
            let position = runner.update(cx, |view, _, cx| {
                let page = view.pdf.as_ref().unwrap().1.read(cx);
                page.page_screen_bounds(0, (0.155, 0.122, 0., 0.)).unwrap().origin
            })?;
            cx.update_window(runner.handle, |_, window, cx| {
                window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position, ..Default::default() }), cx);
                for click_count in 1..=clicks {
                    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { position, click_count, ..Default::default() }), cx);
                    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { position, click_count, ..Default::default() }), cx);
                }
            })?;
            runner.wait_for(cx, "PDF multi-click comment", TIMEOUT_MS, |view, _| view.paper_pdf.selection_comment.is_some()).await?;
            pause(cx, 550).await;
            runner.frame(cx)?;
            runner.update(cx, |view, window, _| assert_comment_only(view, window))?;
        }
        runner.step(Step::ClickPdfCard { card: "lemma".into(), ctrl: false }, cx).await?;
        runner.step(Step::Click { id: Some("edit-pdf-card".into()), x: None, y: None }, cx).await?;
        runner.step(Step::Key { key: "Ctrl+a".into() }, cx).await?;
        runner.step(Step::Type { text: "Unsaved statement".into() }, cx).await?;
        runner.step(Step::ClickPdfCard { card: "lemma".into(), ctrl: true }, cx).await?;
        runner.update(cx, |view, window, _| {
            assert!(view.paper_highlight.as_ref().is_some_and(|(card, _)| card == "lemma"));
            assert!(window.shot_element_bounds("read-head-lemma").is_some());
            assert!(view.paper_pdf.popup.is_none());
            assert_eq!(geometry(window), before);
        })?;
        runner.step(Step::Click { id: Some("save-paper-card".into()), x: None, y: None }, cx).await?;
        runner.update(cx, |view, _, cx| {
            let request = view.card_save_request;
            let body = messages.try_iter().find_map(|message| match message {
                ClientMsg::SaveCard { request: id, card, body, .. }
                    if card == "lemma" && id == request => Some(body),
                _ => None,
            });
            assert_eq!(body.as_deref(), Some("Unsaved statement"));
            view.on_event(ServerEvent::CardSaveResult {
                request, outcome: fuzukue_proto::CardSaveOutcome::Saved,
            }, cx);
            view.open_pdf(path.clone(), None, cx);
        })?;
        runner.step(Step::ClickPdfCard { card: "lemma".into(), ctrl: true }, cx).await?;
        runner.update(cx, |view, _, cx| {
            assert!(view.from_pdf);
            assert!(view.paper_highlight.as_ref().is_some_and(|(card, _)| card == "lemma"));
            assert!(!messages.try_iter().any(|message| matches!(message, ClientMsg::LocatePdf { .. })));
            view.set_paper_pdf(true, cx);
        })?;

        // A click beyond the mapped rectangle still consults SyncTeX in both views.
        for standalone in [false, true] {
            if standalone {
                runner.update(cx, |view, _, cx| view.open_pdf(path.clone(), None, cx))?;
            }
            runner.step(Step::WaitPdf, cx).await?;
            let position = runner.update(cx, |view, _, cx| {
                view.pdf.as_ref().unwrap().1.read(cx)
                    .page_screen_bounds(0, (0.62, 0.122, 0., 0.)).unwrap().origin
            })?;
            runner.ctrl_click_pdf(cx, position)?;
            runner.wait_for(cx, "SyncTeX request", TIMEOUT_MS, |_, _| {
                messages.try_iter().any(|message| matches!(
                    message, ClientMsg::LocatePdf { pdf, page: 0, x, .. }
                        if pdf == path && (x - 0.62).abs() < 0.01
                ))
            }).await?;
            runner.update(cx, |view, window, cx| {
                let card = (!standalone).then(|| "lemma".into());
                view.on_pdf_source_location(path.clone(), "paper.tex".into(), 42, card, window, cx);
                if standalone {
                    assert!(view.right == RightPane::Source);
                } else {
                    assert!(view.right == RightPane::Read && !view.paper_pdf_visible());
                    assert!(view.paper_highlight.as_ref().is_some_and(|(card, _)| card == "lemma"));
                }
            })?;
        }

        runner.update(cx, |view, _, cx| {
            view.open_paper_pdf_artifact(view.deck.clone(), path, cx);
        })?;
        runner.step(Step::WaitPdf, cx).await?;
        check_pdf_cross_page_drag(runner, cx).await?;
        Ok(())
    }

    async fn check_pdf_cross_page_drag(runner: &Runner, cx: &mut AsyncApp) -> Result<()> {
        runner.update(cx, |view, _, cx| {
            let (_, pdf) = view.pdf.as_ref().unwrap();
            pdf.update(cx, |pdf, cx| {
                pdf.replace_bytes(test_pdf_pages(&[("First tail", 1000, 860), ("Second head", 1000, 930)]), cx);
                pdf.set_zoom(0.75, cx);
                pdf.go_to_page(0, cx);
            });
        })?;
        runner.wait_for_with_context(cx, "two-page test PDF", TIMEOUT_MS, |view, _, cx| {
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            pdf.page_ready(0) && pdf.page_screen_bounds(1, (0., 0., 1., 1.)).is_some()
        }).await?;
        let (start, edge) = runner.update(cx, |view, window, cx| {
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            let start = pdf.page_screen_bounds(0, (0.131, 0.135, 0., 0.)).unwrap().origin;
            let viewport = window.shot_element_bounds("pdf-scroll").unwrap();
            assert!(viewport.contains(&start));
            (start, point(start.x, viewport.bottom() - px(2.0)))
        })?;
        cx.update_window(runner.handle, |_, window, cx| {
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: start, ..Default::default() }), cx);
            window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { position: start, click_count: 1, ..Default::default() }), cx);
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: edge, pressed_button: Some(gpui_kit::MouseButton::Left), ..Default::default() }), cx);
        })?;
        let end_point = |view: &Workbench, cx: &Context<Workbench>| {
            let pdf = view.pdf.as_ref().unwrap().1.read(cx);
            pdf.page_screen_bounds(1, (0.30, 0.065, 0., 0.)).unwrap().origin
        };
        // Hold the pointer still. Only drag auto-scroll can make the second
        // page's text visible from this starting position.
        runner.wait_for_with_context(cx, "PDF drag auto-scroll", TIMEOUT_MS, |view, window, cx| {
            end_point(view, cx).y < window.shot_element_bounds("pdf-scroll").unwrap().bottom() - px(40.)
        }).await?;
        let end = runner.update(cx, |view, window, cx| {
            let end = end_point(view, cx);
            assert!(window.shot_element_bounds("pdf-scroll").unwrap().contains(&end));
            end
        })?;
        cx.update_window(runner.handle, |_, window, cx| {
            window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { position: end, click_count: 1, ..Default::default() }), cx);
        })?;
        runner.wait_for_with_context(cx, "two-page PDF copy", TIMEOUT_MS, |view, _, cx| {
            view.pdf.as_ref().unwrap().1.read(cx).selected_text().as_deref() == Some("First tail\n\nSecond head")
        }).await?;
        Ok(())
    }

    #[test]
    fn sidebar_opens_the_remote_browser_and_navigates_with_the_keyboard() {
        use fuzukue_proto::{WorkspaceDirectory, WorkspaceEntry, WorkspaceKind};
        use gpui_kit::{ScrollDelta, ScrollEvent};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, _, _| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                    })?;
                    let click = |cx: &mut AsyncApp, id: &str| -> Result<()> {
                        runner.frame(cx)?;
                        runner.click(cx, ClickTarget::Element { id: id.into() })
                    };
                    let directory = |cx: &mut AsyncApp, path: &str, hidden: bool| -> Result<()> {
                        let ClientMsg::BrowseWorkspace { request, path: requested, hidden: actual } = messages.try_recv()? else {
                            anyhow::bail!("expected directory request");
                        };
                        assert_eq!(requested.trim_end_matches('/'), path);
                        assert_eq!(actual, hidden);
                        runner.update(cx, |view, window, cx| {
                            let entries = if path == "~" {
                                (0..40).map(|n| WorkspaceEntry {
                                    name: if n == 0 { "project".into() } else { format!("folder-{n}") },
                                    directory: true, workspace: n == 0, git: n == 0, fuzukue: n == 0,
                                }).collect()
                            } else {
                                vec![]
                            };
                            view.workspace_directory(request, Ok(WorkspaceDirectory { path: path.into(), entries }), window, cx);
                            cx.notify();
                        })
                    };

                    runner.click_menu(cx, "menu-help", "tool-settings")?;
                    assert!(runner.update(cx, |view, _, _| view.settings_open)?);
                    click(cx, "close-settings")?;
                    click(cx, "workspace-account")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |view, window, _| {
                        !view.settings_open && window.shot_element_bounds("workspace-account-menu").is_some()
                    })?);
                    runner.key(cx, "escape")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("workspace-account-menu").is_none())?);
                    click(cx, "workspace-account")?;
                    click(cx, "tool-search")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("workspace-account-menu").is_none())?);
                    runner.key(cx, "escape")?;
                    click(cx, "workspace-account")?;
                    click(cx, "account-server")?;
                    assert!(runner.update(cx, |view, _, _| view.settings_open && view.sidebar.server_settings)?);
                    assert!(matches!(messages.try_recv()?, ClientMsg::SelfUpdateStatus));
                    click(cx, "close-settings")?;
                    click(cx, "workspace-account")?;
                    click(cx, "open-workspace")?;
                    directory(cx, "~", false)?;
                    runner.frame(cx)?;
                    runner.key(cx, "down")?;
                    runner.frame(cx)?;
                    let bounds = runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("workspace-directory-list").unwrap()
                    })?;
                    let scroll = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| view.workspace_browser_scroll());
                    assert_eq!(scroll(cx)?, px(0.));
                    let thumb = point(bounds.right() - px(6.), bounds.top() + px(20.));
                    runner.drag(cx, thumb, thumb + point(px(0.), px(60.)))?;
                    runner.frame(cx)?;
                    let dragged = scroll(cx)?;
                    assert!(dragged < px(0.), "scrollbar drag should move the directory list");
                    runner.update(cx, |_, window, cx| {
                        window.dispatch_event(PlatformInput::Scroll(ScrollEvent {
                            position: bounds.center(),
                            delta: ScrollDelta::Lines(point(0., -5.)),
                            ..Default::default()
                        }), cx);
                    })?;
                    runner.frame(cx)?;
                    let scrolled = scroll(cx)?;
                    assert!(scrolled < dragged, "wheel should scroll with a row selected");
                    runner.key(cx, "up")?;
                    runner.frame(cx)?;
                    assert_eq!(scroll(cx)?, scrolled, "the same selection should leave scroll alone");
                    runner.key(cx, "down")?;
                    runner.frame(cx)?;
                    assert!(scroll(cx)? > scrolled, "keyboard selection should reveal the new row");
                    runner.key(cx, "up")?;
                    runner.key(cx, "enter")?;
                    directory(cx, "~/project", false)?;
                    runner.frame(cx)?;
                    runner.key(cx, "backspace")?;
                    directory(cx, "~", false)?;
                    click(cx, "workspace-hidden")?;
                    directory(cx, "~", true)?;
                    click(cx, "workspace-path-input")?;
                    runner.key(cx, "Ctrl+A")?;
                    runner.type_text(cx, "~/project")?;
                    runner.key(cx, "enter")?;
                    directory(cx, "~/project", true)?;

                    click(cx, "workspace-create")?;
                    runner.frame(cx)?;
                    runner.type_text(cx, "draft")?;
                    click(cx, "workspace-paper")?;
                    click(cx, "workspace-create-confirm")?;
                    let ClientMsg::CreateWorkspace { request, parent, name, kind } = messages.try_recv()? else {
                        anyhow::bail!("expected workspace creation request");
                    };
                    assert_eq!((parent.as_str(), name.as_str(), kind), ("~/project/", "draft", WorkspaceKind::Paper));
                    runner.update(cx, |view, window, cx| {
                        view.workspace_created(request, Ok("~/project/draft".into()), window, cx);
                    })?;
                    directory(cx, "~/project/draft", true)?;
                    click(cx, "confirm-open-workspace")?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::OpenWorkspace { path } if path == "~/project/draft/"));
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn sidebar_reorders_threads_with_drag_and_keys_and_archives_on_drop() {
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    let settings = tempfile::tempdir()?;
                    let path = settings.path().join("settings.toml");
                    runner.update(cx, |view, _, cx| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.appearance.isolate_shot_settings(path.clone());
                        view.workspaces.active = Some("test-workspace".into());
                        let threads = ["a", "b", "c"].map(|id| {
                            serde_json::from_value(serde_json::json!({"id": id, "title": id, "updated": 0})).unwrap()
                        }).to_vec();
                        view.on_event(ServerEvent::Threads { threads }, cx);
                        view.select_thread("b".into(), cx);
                        view.thread_info.plans.insert("a".into(), (fuzukue_proto::Plan {
                            goal: "Test goal".into(), done_condition: String::new(),
                            status: fuzukue_proto::PlanStatus::Active, pace: None,
                            done: vec![], next: vec![], rejected: vec![], question: None,
                            verification: None, paused_by_user: false,
                        }, None));
                    })?;
                    let bounds = |cx: &mut AsyncApp, id: &str| {
                        runner.update(cx, |_, window, _| window.shot_element_bounds(id).unwrap())
                    };
                    let hold = |cx: &mut AsyncApp, from, to| {
                        cx.update_window(runner.handle, |_, window, cx| {
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position: from, ..Default::default()
                            }), cx);
                            window.draw(cx).clear(cx);
                            window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                                position: from, click_count: 1, ..Default::default()
                            }), cx);
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position: from + point(px(1.), px(0.)),
                                pressed_button: Some(gpui_kit::MouseButton::Left), ..Default::default()
                            }), cx);
                            assert!(!cx.has_active_drag(), "one pixel of movement must preserve a click");
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position: to, pressed_button: Some(gpui_kit::MouseButton::Left),
                                ..Default::default()
                            }), cx);
                            window.draw(cx).clear(cx);
                            cx.has_active_drag()
                        })
                    };
                    let release = |cx: &mut AsyncApp, position| {
                        cx.update_window(runner.handle, |_, window, cx| {
                            window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                                position, click_count: 1, ..Default::default()
                            }), cx);
                        })
                    };
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "thread-row-title-a".into() })?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |view, _, cx| view.active == "a" && !cx.has_active_drag())?);

                    use crate::sidebar::{SIDEBAR_ICON_GAP, SIDEBAR_ROW_INSET};
                    let icon = bounds(cx, "thread-row-icon-a")?;
                    assert_eq!(icon.left(), bounds(cx, "tool-search")?.left() + SIDEBAR_ROW_INSET);
                    assert_eq!(bounds(cx, "thread-row-title-a")?.left(), icon.right() + SIDEBAR_ICON_GAP);
                    let a = bounds(cx, "thread-row-title-a")?.center();
                    assert!(runner.update(cx, |_, window, _| {
                        window.shot_element_bounds("archived-threads-toggle").is_none()
                            && window.shot_element_bounds("archived-threads-drop-zone").is_none()
                    })?);
                    let drag_point = a + point(px(20.), px(0.));
                    assert!(hold(cx, a, drag_point)?);
                    let archive = bounds(cx, "archived-threads-drop-zone")?.center();
                    let preview = bounds(cx, "thread-drag-preview")?;
                    let icon = bounds(cx, "thread-drag-preview-icon")?;
                    let title = bounds(cx, "thread-drag-preview-title")?;
                    let offset = preview.origin - drag_point;
                    assert!((offset.x - px(8.)).abs() < px(1.) && (offset.y - px(8.)).abs() < px(1.),
                        "preview should be eight pixels below and right of the pointer, got {offset:?}");
                    assert_eq!(icon.left(), preview.left() + SIDEBAR_ROW_INSET + px(1.));
                    assert_eq!(title.left(), icon.right() + SIDEBAR_ICON_GAP);

                    runner.key(cx, "Escape")?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |_, window, cx| !cx.has_active_drag()
                        && window.shot_element_bounds("thread-drag-preview").is_none()
                        && window.shot_element_bounds("archived-threads-drop-zone").is_none())?);
                    release(cx, archive)?;

                    let outside = archive + point(px(350.), px(0.));
                    assert!(hold(cx, a, outside)?);
                    release(cx, outside)?;
                    runner.frame(cx)?;
                    assert!(runner.update(cx, |view, _, cx| !cx.has_active_drag()
                        && view.thread_manual_order().is_empty())?);

                    let row = bounds(cx, "thread-row-all-a")?;
                    for control in [bounds(cx, "thread-goal-marker-a")?.center(),
                        bounds(cx, "thread-menu-a")?.center(), row.left_center() + point(px(1.), px(0.))]
                    {
                        assert!(!hold(cx, control, archive)?);
                        release(cx, archive)?;
                    }
                    assert!(!messages.try_iter().any(|message| matches!(message, ClientMsg::ArchiveThread { .. })));

                    let c = bounds(cx, "thread-row-all-c")?;
                    runner.drag(cx, a, point(c.center().x, c.bottom() - px(3.)))?;
                    runner.frame(cx)?;
                    let order = |cx: &mut AsyncApp| runner.update(cx, |view, _, _| view.thread_manual_order().to_vec());
                    assert_eq!(order(cx)?, ["b", "c", "a"]);

                    runner.click(cx, ClickTarget::Element { id: "thread-row-title-a".into() })?;
                    runner.key(cx, "Alt+Up")?;
                    runner.frame(cx)?;
                    assert_eq!(order(cx)?, ["b", "a", "c"]);
                    let settings = crate::settings_file::read(&path.with_extension("state.toml"))?;
                    let saved = settings["sidebar"]["manual_order"]["test-workspace"].as_array().unwrap();
                    assert_eq!(saved.iter().map(|id| id.as_str().unwrap()).collect::<Vec<_>>(), ["b", "a", "c"]);

                    let a = bounds(cx, "thread-row-icon-a")?.center();
                    assert!(hold(cx, a, a + point(px(20.), px(0.)))?);
                    let archive = bounds(cx, "archived-threads-drop-zone")?.center();
                    release(cx, archive)?;
                    assert!(messages.try_iter().any(|message| matches!(message,
                        ClientMsg::ArchiveThread { id, archived: true } if id == "a")));
                    runner.update(cx, |view, _, cx| {
                        let mut threads = view.threads.clone();
                        threads.iter_mut().find(|thread| thread.id == "a").unwrap().archived = true;
                        view.on_event(ServerEvent::Threads { threads }, cx);
                    })?;
                    runner.frame(cx)?;
                    bounds(cx, "archived-threads-toggle")?;

                    // Clear the in-memory sidebar so the next render loads preferences from disk.
                    runner.update(cx, |view, _, cx| {
                        view.sidebar = Default::default();
                        cx.notify();
                    })?;
                    runner.frame(cx)?;
                    assert_eq!(order(cx)?, ["b", "a", "c"]);
                    Ok(())
                })();
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn clicking_workspace_rows_restores_a_previously_selected_thread() {
        use fuzukue_proto::{Envelope, WorkspaceInfo};

        fn threads(ids: &[&str]) -> ServerEvent {
            ServerEvent::Threads { threads: ids.iter().enumerate().map(|(index, id)| ThreadView {
                            github: Default::default(),
                            untitled: false,
                            auto_attached: Vec::new(), suggestions: Vec::new(),
                id: (*id).to_owned(), title: (*id).to_owned(), updated: index as i64,
                archived: false, pinned: false, attached: vec![], math_macros: vec![], artifacts: vec![], claude_session: None, partner_settings: None, agent_overrides: Default::default(),
                    worker_settings: None,
            }).collect() }
        }

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            cx.spawn(async move |cx| {
                let runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome = (|| -> Result<()> {
                    runner.update(cx, |view, window, cx| {
                        view.out = out.into();
                        view.protocol = fuzukue_proto::PROTOCOL_VERSION;
                        view.workspaces.entries = ["first", "second"].into_iter().map(|id| WorkspaceInfo {
                            id: id.into(), title: id.into(), path: format!("/home/test/{id}"),
                            last_opened: Some(0), running: false,
                        }).collect();
                        view.on_conn(crate::conn::ConnEvent::Event(Envelope {
                            seq: 0, workspace: None,
                            event: ServerEvent::WorkspaceSelected { id: "first".into(), replay: true },
                        }), window, cx);
                        view.on_conn(crate::conn::ConnEvent::Event(Envelope {
                            seq: 0, workspace: Some("first".into()),
                            event: threads(&["older", "latest"]),
                        }), window, cx);
                        assert!(view.threads_loaded);
                        view.switch_session("older".into(), cx);
                        let pdf = "paper/build/full.pdf";
                        view.pdf_path = Some(pdf.into());
                        view.pdf_digest = Some("first".into());
                        view.receive_paper_pdf_map("paper".into(), pdf.into(), "first".into(), vec![], true, cx);
                        assert!(view.pdf_regions().is_some());
                    })?;
                    while messages.try_recv().is_ok() {}
                    runner.frame(cx)?;
                    runner.click(cx, ClickTarget::Element { id: "workspace-second".into() })?;
                    assert!(matches!(messages.try_recv()?, ClientMsg::SwitchWorkspace { id } if id == "second"));

                    runner.update(cx, |view, window, cx| {
                        view.workspace_selected("second".into(), true, window, cx);
                        assert!(view.pdf_path.is_none() && view.pdf_digest.is_none());
                        assert!(view.built_pdfs.is_empty());
                        view.on_conn(crate::conn::ConnEvent::Event(Envelope {
                            seq: 0, workspace: Some("first".into()),
                            event: ServerEvent::PaperPdfMap {
                                deck: "paper".into(), pdf: "paper/build/full.pdf".into(),
                                digest: "first".into(), regions: vec![], rebuilt: true,
                            },
                        }), window, cx);
                        assert!(view.built_pdfs.is_empty());
                        view.on_event(threads(&["their-thread"]), cx);
                        assert_eq!(view.active, "their-thread");
                        view.workspace_selected("first".into(), true, window, cx);
                        view.on_event(threads(&["older", "latest"]), cx);
                        assert_eq!(view.active, "older");
                        view.out.send(ClientMsg::NewThread).unwrap();
                    })?;
                    assert!(messages.try_iter().any(|msg| matches!(
                        msg,
                        ClientMsg::InWorkspace { id, request }
                            if id == "first" && matches!(*request, ClientMsg::NewThread)
                    )));
                    Ok(())
                })();
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn ctrl_click_on_paper_requests_a_definition_and_opens_cross_deck_references() {
        use fuzukue_proto::{DocItem, DocSection, SymbolHit, SymbolKind, SymbolLocation, SymbolLookup, SymbolReference, SymbolResults};

        fn paper(view: &mut Workbench, deck: &str, id: &str, body: &str, cx: &mut Context<Workbench>) {
            view.on_event(ServerEvent::Workspace { tabs: vec![], deck: deck.into() }, cx);
            view.on_event(ServerEvent::Cards {
                deck: deck.into(),
                diagnostics: vec![],
                cards: vec![serde_json::from_value(serde_json::json!({
                    "id":id,"kind":"definition","title":id,"status":"","path":format!("{deck}/cards/{id}.md"),
                    "links":[],"backlinks":[],"files":[],"body":body,"external":false
                })).unwrap()],
            }, cx);
            view.on_event(ServerEvent::Document {
                title: "Paper".into(), macros: vec![], math_macros: vec![], references: None,
                sections: vec![DocSection { title: "Section".into(), items: vec![DocItem::Card { id: id.into() }], prose: vec![], unmatched: false }],
            }, cx);
        }

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.out = out.into();
                view.protocol = fuzukue_proto::SYMBOL_PROTOCOL;
                view.right = RightPane::Read;
                paper(view, "papers/right", "lemma", r"See \ref{thm:main} and a median. A formula $\foo{x}$.", cx);
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    for text in ["thm:main", r"\foo", "median"] {
                        runner.step(Step::CtrlClickText { card: "lemma".into(), text: text.into() }, cx).await?;
                        let ClientMsg::LookupSymbol { request, lookup: SymbolLookup::At { deck, card, text: source, start, end } } = messages.try_recv()?
                        else { bail!("Ctrl+click did not request a definition") };
                        assert_eq!((deck.as_str(), card.as_str()), ("papers/right", "lemma"));
                        let clicked = &source[start..end];
                        if text == "median" {
                            assert_eq!(clicked.chars().count(), 1);
                            assert!(text.contains(clicked));
                            let location = SymbolLocation { deck: "papers/left".into(), file: "papers/left/cards/median.md".into(),
                                card: Some("median".into()), start: 0, end: 6, line: 1, end_line: None, snippet: "A median".into() };
                            let definitions = vec![SymbolHit { id: "test".into(), kind: SymbolKind::Term, name: "median".into(),
                                qualified: "papers/left#median".into(), origin: "card_defines".into(), location: location.clone(), alternates: vec![] }];
                            runner.update(cx, |view, window, cx| {
                                view.on_symbol_results(request, SymbolResults { definitions, ..Default::default() }, window, cx);
                            })?;
                            assert!(matches!(messages.try_recv()?, ClientMsg::SelectPaper { deck } if deck == "papers/left"));
                            let ClientMsg::LookupSymbol { request, lookup: SymbolLookup::References { id } } = messages.try_recv()?
                            else { bail!("reference request missing") };
                            assert_eq!(id, "test");
                            runner.update(cx, |view, window, cx| {
                                paper(view, "papers/left", "median", "Definition.", cx);
                                let references = vec![SymbolReference {
                                    location: SymbolLocation { deck: "papers/right".into(), snippet: "See a median.".into(), ..location },
                                    alternates: vec![],
                                }];
                                view.on_symbol_results(request, SymbolResults { references, ..Default::default() }, window, cx);
                            })?;
                        } else {
                            assert!(clicked.contains(text), "{text} mapped to {clicked}");
                        }
                    }
                    runner.step(Step::WaitPaperCard { deck: "papers/left".into(), card: "median".into(), text: None }, cx).await?;
                    runner.step(Step::WaitElement { id: "symbol-reference-0".into() }, cx).await?;
                    Ok(())
                }.await;
                let _ = result.send(outcome);
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

    #[test]
    fn paper_card_and_pdf_views_use_real_input_and_preserve_the_layout() {
        use fuzukue_proto::{CardEditPart, CardSaveOutcome, DocItem, DocSection};

        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                math::init(cx);
                let (handle, view) = open_workbench(cx).unwrap();
                let (out, messages) = std::sync::mpsc::channel();
                view.update(cx, |view, cx| {
                    view.switch_session("thread-1".into(), cx);
                    view.out = out.into();
                    view.protocol = 9;
                    view.on_event(ServerEvent::OpenPaper {
                        session: "inactive".into(), deck: "papers/other".into(), card: None,
                    }, cx);
                    assert!(view.right == RightPane::None);
                    assert!(messages.try_recv().is_err());

                    view.on_event(ServerEvent::OpenPaper {
                        session: "thread-1".into(), deck: "papers/test".into(), card: Some("lemma".into()),
                    }, cx);
                    assert!(matches!(messages.try_recv().unwrap(), ClientMsg::SelectPaper { deck } if deck == "papers/test"));
                    view.on_event(ServerEvent::Workspace { tabs: vec![], deck: "papers/test".into() }, cx);
                    let card = serde_json::from_str(r#"{
                        "id":"lemma","kind":"lemma","title":"補題","status":"","path":"cards/lemma.md",
                        "links":[],"backlinks":[],"files":[],"body":"旧本文","external":false,
                        "edit_body":{"body":"旧本文","base_hash":"original"}
                    }"#).unwrap();
                    view.on_event(ServerEvent::Cards {
                        deck: "papers/test".into(), cards: vec![card], diagnostics: vec![],
                    }, cx);
                    view.on_event(ServerEvent::Document {
                        title: "論文".into(), macros: vec![], math_macros: vec![], references: None,
                        sections: vec![DocSection { title: "節".into(), items: vec![DocItem::Card { id: "lemma".into() }], prose: vec![], unmatched: false }],
                    }, cx);
                });
                cx.spawn(async move |cx| {
                    let mut runner = Runner {
                        handle,
                        view,
                        target: None,
                        output: PathBuf::new(),
                        read_only: false,
                        seen: HashMap::new(),
                    };
                    let outcome: Result<()> = async {
                        let steps: Scenario = serde_json::from_str(r#"{"steps":[
                            {"action":"wait_paper_card","deck":"papers/test","card":"lemma"},
                            {"action":"click","id":"edit-paper-statement-lemma"},
                            {"action":"key","key":"Ctrl+a"},
                            {"action":"type","text":"変更した本文\n二行目"},
                            {"action":"key","key":"Ctrl+Enter"}
                        ]}"#)?;
                        for step in steps.steps {
                            runner.step(step, cx).await?;
                        }
                        let ClientMsg::SaveCard { request, deck, card, part, body, base_hash } = messages.try_recv()?
                        else { bail!("Ctrl+Enter did not request a card save") };
                        assert_eq!((deck.as_str(), card.as_str(), part, body.as_str(), base_hash.as_str()),
                            ("papers/test", "lemma", CardEditPart::Body, "変更した本文\n二行目", "original"));
                        runner.update(cx, |view, _, cx| {
                            view.on_event(ServerEvent::CardSaveResult { request, outcome: CardSaveOutcome::Error {
                                message: "保存できない".into(),
                            }}, cx);
                            assert!(view.card_editor("lemma", false).is_some());
                        })?;
                        runner.step(Step::Key { key: "escape".into() }, cx).await?;
                        runner.frame(cx)?;
                        runner.update(cx, |view, _, _| {
                            assert!(view.card_editor("lemma", false).is_some());
                        })?;
                        runner.click(cx, ClickTarget::Element { id: "discard-paper-card".into() })?;
                        runner.update(cx, |view, _, _| {
                            assert!(view.card_editor("lemma", false).is_none());
                            view.protocol = 10;
                        })?;
                        check_paper_pdf(&mut runner, &messages, cx).await?;
                        Ok(())
                    }.await;
                    let _ = result.send(outcome);
                    cx.update(|cx| cx.quit());
                }).detach();
            });
        completed.recv().unwrap().unwrap();
    }


    #[test]
    fn change_history_buttons_open_card_and_turn_diffs_and_resolve_conflicts() {
        use fuzukue_proto::{DocItem, DocSection, HistoryOutcome, HistoryRecord, HistoryRequest, HistoryScope};
        let (result, completed) = std::sync::mpsc::channel();
        gpui_kit::platform::headless().with_assets(gpui_kit::assets::Assets).run(move |cx| {
            gpui_kit::init(cx);
            math::init(cx);
            let (handle, view) = open_workbench(cx).unwrap();
            let (out, messages) = std::sync::mpsc::channel();
            view.update(cx, |view, cx| {
                view.switch_session("thread-1".into(), cx);
                view.out = out.into();
                view.protocol = fuzukue_proto::CHANGE_HISTORY_PROTOCOL;
                view.on_event(ServerEvent::OpenPaper {
                    session: "thread-1".into(), deck: "papers/test".into(), card: Some("lemma".into()),
                }, cx);
                view.on_event(ServerEvent::Workspace { tabs: vec![], deck: "papers/test".into() }, cx);
                let card = serde_json::from_str(r#"{
                    "id":"lemma","kind":"lemma","title":"補題","status":"","path":"cards/lemma.md",
                    "links":[],"backlinks":[],"files":[],"body":"新本文","external":false
                }"#).unwrap();
                view.on_event(ServerEvent::Cards {
                    deck: "papers/test".into(), cards: vec![card], diagnostics: vec![],
                }, cx);
                view.on_event(ServerEvent::Document {
                    title: "論文".into(), macros: vec![], math_macros: vec![], references: None,
                    sections: vec![DocSection { title: "節".into(), items: vec![DocItem::Card { id: "lemma".into() }], prose: vec![], unmatched: false }],
                }, cx);
                for (item, text) in [("reply-read-only", "変更なし。"), ("silent", "<silent />"), ("reply-1", "本文を補った。")] {
                    view.on_event(ServerEvent::TextDelta {
                        session: "thread-1".into(), item: item.into(), actor: fuzukue_proto::Actor::Claude,
                        text: text.into(),
                    }, cx);
                    view.on_event(ServerEvent::TurnEnded {
                        session: "thread-1".into(), actor: fuzukue_proto::Actor::Claude,
                    }, cx);
                }
            });
            cx.spawn(async move |cx| {
                let mut runner = Runner { handle, view, target: None, output: PathBuf::new(), read_only: false, seen: HashMap::new() };
                let outcome: Result<()> = async {
                    let record = HistoryRecord {
                        id: "record-1".into(), actor: "agent:claude".into(), thread: Some("thread-1".into()),
                        turn: Some("turn-1".into()), items: vec!["reply-1".into()], time_ms: 123,
                        files: vec!["papers/test/cards/lemma.md".into()], summary: "本文を補った。".into(), reverts: None, branch: None,
                    };
                    let receive = || -> Result<_> {
                        while let Ok(message) = messages.try_recv() {
                            if let ClientMsg::History { id, request } = message {
                                return Ok((id, request));
                            }
                        }
                        bail!("History request was not sent")
                    };
                    let click = |id: &str| Step::Click { id: Some(id.into()), x: None, y: None };
                    runner.step(Step::WaitPaperCard { deck: "papers/test".into(), card: "lemma".into(), text: None }, cx).await?;
                    runner.step(click("paper-history-lemma"), cx).await?;
                    let (id, request) = receive()?;
                    assert!(matches!(request, HistoryRequest::List { scope: HistoryScope::Card { deck, card } }
                        if deck == "papers/test" && card == "lemma"));
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryResult {
                        id, outcome: HistoryOutcome::Records { records: vec![record.clone()] },
                    }, cx))?;
                    runner.step(click("history-record-0"), cx).await?;
                    let (id, request) = receive()?;
                    assert!(matches!(request, HistoryRequest::Diff { record } if record == "record-1"));
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryResult {
                        id, outcome: HistoryOutcome::Diff { text: "-旧本文
+新本文".into(), truncated: false },
                    }, cx))?;
                    runner.step(Step::WaitElement { id: "history-diff".into() }, cx).await?;
                    runner.step(click("history-revert"), cx).await?;
                    let (id, request) = receive()?;
                    assert!(matches!(request, HistoryRequest::Revert { record, clean_only: None } if record == "record-1"));
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryResult {
                        id, outcome: HistoryOutcome::Conflict { hunks: vec!["-新本文".into()], clean_only: Some("preview".into()) },
                    }, cx))?;
                    runner.step(click("history-cancel"), cx).await?;
                    assert!(receive().is_err());
                    runner.step(click("history-revert"), cx).await?;
                    let (id, _) = receive()?;
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryResult {
                        id, outcome: HistoryOutcome::Conflict { hunks: vec!["-新本文".into()], clean_only: Some("preview".into()) },
                    }, cx))?;
                    runner.step(click("history-clean-only"), cx).await?;
                    assert!(matches!(receive()?.1, HistoryRequest::Revert { clean_only: Some(token), .. } if token == "preview"));
                    runner.step(click("history-close"), cx).await?;
                    runner.step(Step::WaitElement { id: "chat-copy-2".into() }, cx).await?;
                    runner.step(Step::WaitElement { id: "chat-save-memo-2".into() }, cx).await?;
                    assert!(runner.update(cx, |_, window, _| {
                        let bubble = window.shot_element_bounds("chat-message-bubble-2").unwrap();
                        let memo = window.shot_element_bounds("chat-save-memo-2").unwrap();
                        let details = window.shot_element_bounds("message-details-2").unwrap();
                        window.shot_element_bounds("chat-message-bubble-0").is_some()
                            && window.shot_element_bounds("chat-copy-0").is_none()
                            && window.shot_element_bounds("chat-message-bubble-1").is_none()
                            && window.shot_element_bounds("message-details-1").is_none()
                            && memo.origin.y >= bubble.origin.y + bubble.size.height
                            && details.origin.y == memo.origin.y
                            && (details.origin.x + details.size.width - bubble.origin.x - bubble.size.width).abs() < px(1.)
                    })?);
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("turn-history-reply-1").is_none())?);
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryChanged {
                        changed_turns: Some([("thread-1".into(), ["turn-1".into(), "reply-1".into()].into())].into()),
                    }, cx))?;
                    assert!(runner.update(cx, |_, window, _| window.shot_element_bounds("turn-history-reply-read-only").is_none())?);
                    runner.step(Step::WaitElement { id: "turn-history-reply-1".into() }, cx).await?;
                    runner.step(click("turn-history-reply-1"), cx).await?;
                    let (id, request) = receive()?;
                    assert!(matches!(request, HistoryRequest::List { scope: HistoryScope::Turn { thread, turn } }
                        if thread == "thread-1" && turn == "reply-1"));
                    runner.update(cx, |view, _, cx| view.on_event(ServerEvent::HistoryResult {
                        id, outcome: HistoryOutcome::Records { records: vec![record] },
                    }, cx))?;
                    assert!(matches!(receive()?.1, HistoryRequest::Diff { record } if record == "record-1"));
                    Ok(())
                }.await;
                result.send(outcome).unwrap();
                cx.update(|cx| cx.quit());
            }).detach();
        });
        completed.recv().unwrap().unwrap();
    }

}
