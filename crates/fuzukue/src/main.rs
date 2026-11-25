//! fuzukue: Windows 側の GPUI クライアント。
//! 左: スレッド / 中央: エージェントとのチャット / 右: 論文、カード、設定。

// リリースビルドではコンソールの窓を出さない（デバッグビルドはログを見るために残す）。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[macro_use]
mod i18n;

mod agent_management;
mod agent_models;
mod appearance;
mod approval_dock;
mod notifications;
mod card_reading;
mod chat;
mod chat_components;
mod chat_question;
mod chat_quote;
mod branch_review;
mod handoff;
mod change_history;
mod unified_diff;
mod conn;
mod composer_markdown;
mod deck_overview;
mod lazy_deck;
mod github_report;
#[cfg(test)]
mod cjk_emphasis_tests;
mod icons;
mod library;
mod menu_bar;
mod settings_file;
mod review_findings;
mod review_reports;
mod extensions;
mod exploration;
mod math;
mod paper;
mod paper_edit;
mod paper_pdf;
mod pane_resize;
mod project_tests;
mod safe_close;
mod server;
mod self_update;
mod screenshot;
mod sidebar;
mod workspace_requests;
#[cfg(all(target_os = "linux", feature = "headless-shot"))]
mod shot;
mod source;
mod file_browser;
mod source_outline;
mod style_notes;
mod symbols;
mod threads;
mod trash;
mod thread_info;
mod distribution;
mod tailnet;
mod updater;
mod workspace_git;
mod workspaces;
mod workshop;
mod issue_threads;
mod translation;

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use gpui_pdf::PdfView;

use futures::StreamExt;
use gpui_kit::component::{
    h_flex,
    scroll::ScrollableElement as _,
    text::TextView,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    v_flex, ActiveTheme as _, WindowExt as _,
};
use gpui_kit::base::input::Enter;
use gpui_kit::base::text::TextViewState;
use gpui_kit::base::{TextSelection, TextSelectionScopeId};
use gpui_kit::Focusable;
use gpui_kit::{
    div, prelude::*, px, relative, AnyElement, App, Context, Entity, EntityInputHandler, MouseButton, Pixels, Point, SharedString, Task, Window,
};
use fuzukue_proto::{CardDiagnostic, CardView, ClientMsg, CommentSource, CommentView, DocReferences, DocSection, IssueView, ResearchOrigin, ServerEvent, TabView, ThreadView, UsageWindow};

use fuzukue_view::{parse_source, expand_macros, kind_rank, link_card_refs, paper_markdown, read_card_tex, truncate, TexMarkdown};

use crate::appearance::Appearance;
use crate::chat::Chat;
use crate::conn::{ConnEvent, HttpTarget};
use crate::icons::Symbol;
use crate::pane_resize::PANE_INSET;
use crate::paper::paper_list;
use crate::source::SourcePane;

enum ConnectionStatus {
    Connecting,
    Connected,
    Reconnecting(String),
    Switching,
    ServerSwap,
    Detail(String),
}

impl ConnectionStatus {
    fn text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Connecting => tr!("workbench-connecting").into(),
            Self::Connected => tr!("workbench-connected").into(),
            Self::Reconnecting(detail) => tr!("workbench-reconnecting", e = detail.as_str()).into(),
            Self::ServerSwap => tr!("self-update-notice").into(),
            Self::Switching => tr!("settings-switching-connection").into(),
            Self::Detail(detail) => detail.as_str().into(),
        }
    }
}

struct Workbench {
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    shot_activity: Option<shot::Activity>,
    appearance: Appearance,
    notifications: notifications::Notifications,
    approvals: approval_dock::ApprovalDock,
    settings_open: bool,
    trash_items: Option<Vec<fuzukue_proto::TrashItem>>,
    trash_error: Option<String>,
    extensions: extensions::Extensions,
    sidebar: sidebar::Sidebar,
    thread_info: thread_info::ThreadInfo,
    distribution: distribution::Distribution,
    self_update: self_update::Panel,
    screenshots: screenshot::Screenshots,
    history: change_history::History,
    branch_review: branch_review::BranchReview,
    handoffs: handoff::Handoffs,
    chats: HashMap<String, Chat>,
    workspace_requests: workspace_requests::WorkspaceRequests,
    recent_transcripts: VecDeque<String>,
    question_drafts: HashMap<String, Entity<chat_question::QuestionCard>>,
    chat_capabilities: Option<chat::ChatCapabilities>,
    codex_models: Vec<fuzukue_proto::CodexModel>,
    delegate_settings: Option<fuzukue_proto::DelegateSettings>,
    worker_approval_policy: Option<fuzukue_proto::WorkerApprovalPolicy>,
    agent_settings: fuzukue_proto::AgentSettings,
    agent_management: agent_management::AgentManagement,
    agent_models: agent_models::ModelSettings,
    review_settings: Option<fuzukue_proto::ReviewSettings>,
    partner_default: Option<fuzukue_proto::ChatSettings>,
    /// 選択しているスレッド。空なら会話は未選択。
    active: String,
    issues: Vec<IssueView>,
    repo: String,
    issue_error: Option<String>,
    deck_overview: deck_overview::DeckOverview,
    workshop: workshop::Workshop,
    exploration: exploration::Pane,
    github: issue_threads::GitHub,
    github_report: github_report::GitHubReport,
    input: Entity<TextareaState>,
    composer_markdown: composer_markdown::ComposerMarkdown,
    out: conn::Sender,
    workspaces: workspaces::WorkspaceState,
    receiving_backlog: bool,
    transcripts: chat::Transcripts,
    connection: conn::Connection,
    safe_close: safe_close::SafeClose,
    status: ConnectionStatus,
    cards: Vec<CardView>,
    card_diags: Vec<CardDiagnostic>,
    lazy_deck: lazy_deck::LazyDeck,
    style_note_disclosure: fuzukue_view::style_notes::StyleNoteDisclosure,
    selected: Option<String>,
    pdfs: Vec<String>,
    right: RightPane,
    reader_selection_scope: TextSelectionScopeId,
    pane_resize: pane_resize::PaneResize,
    /// 開いている PDF（サーバー上の相対パス）と、そのビュー
    pdf: Option<(String, Entity<PdfView>)>,
    pdf_status: Option<SharedString>,
    pdf_path: Option<String>,
    pdf_request: u64,
    pdf_digest: Option<String>,
    http: HttpTarget,
    connection_epoch: u64,
    usage: Vec<UsageWindow>,
    pause_at: f32,
    /// PDF から飛んできたカードなら、PDF に戻るボタンを出す
    from_pdf: bool,
    /// 編集中のファイル（プロジェクトからの相対パス）と、その入力欄
    editing: Option<source::card::CardFileEditor>,
    /// 論文の並び（structure.yaml）とマクロ
    doc_title: String,
    doc_sections: Vec<DocSection>,
    document_deck: Option<String>,
    doc_macros: Vec<(String, String)>,
    doc_math_macros: Vec<fuzukue_proto::MathMacro>,
    doc_references: Option<DocReferences>,
    /// 通して読む画面の行と、カードごとの選択可能な Markdown
    read_rows: Vec<ReadRow>,
    read_md: HashMap<ReadPart, ReadText>,
    open_proofs: HashMap<String, HashSet<String>>,
    proof_comments: HashSet<String>,
    paper_jump: Option<PaperJump>,
    reviews: review_reports::Pane,
    findings: review_findings::Findings,
    pending_paper_open: Option<(String, Option<String>, bool)>,
    paper_highlight: Option<(String, gpui_kit::Task<()>)>,
    card_editor: Option<paper_edit::CardEditor>,
    card_tex_build: Option<paper_edit::CardTexBuild>,
    card_save_request: u64,
    translation_save_request: Option<u64>,
    paper_jobs: std::collections::BTreeMap<(String, Option<String>), String>,
    paper_job_result: Option<paper::PaperJobResult>,
    pdf_audit: Vec<fuzukue_proto::PdfAudit>,
    review_jobs: std::collections::BTreeMap<(String, String), paper::ReviewProgress>,
    review_clock: Option<gpui_kit::Task<()>>,
    chat_activity_clock: Option<gpui_kit::Task<()>>,
    chat_waiting: chat::ChatWaitingState,
    built_pdfs: HashMap<String, String>,
    paper_pdf: paper_pdf::PaperPdf,
    protocol: u32,
    read_list: gpui_kit::ListState,
    paper_focus: gpui_kit::FocusHandle,
    chat_focus: gpui_kit::FocusHandle,
    view_focus: gpui_kit::FocusHandle,
    app_menu: menu_bar::MenuBar,
    _close_key: gpui_kit::Subscription,
    /// 検索・添付の参照先、および概要の表示に使うデッキ情報。
    tabs: Vec<TabView>,
    workspace_title: Option<String>,
    threads: Vec<ThreadView>,
    threads_loaded: bool,
    thread_title_edit: Option<(String, Entity<InputState>)>,
    auto_prompt: Option<String>,
    pending_research_origin: Option<ResearchOrigin>,
    active_tab: Option<String>,
    deck: String,
    /// 右ペインに開いているノート（パス, 本文）
    note: Option<(String, String)>,
    source: SourcePane,
    files: file_browser::Files,
    tests: project_tests::TestsPane,
    symbols: symbols::SymbolPane,
    library: library::LibraryPane,
    /// Lean の宣言 → 状態（"proved" / "defined" / "sorry" / "missing" / …）
    lean: HashMap<String, String>,
    lean_running: bool,
    lean_doc_findings: Vec<fuzukue_proto::LeanDocFinding>,
    /// 今のデッキ（論文）にためているコメント
    comments: Vec<CommentView>,
    /// コメントを書いているカードと、その入力欄
    comment_target: Option<CommentTarget>,
    comment_input: Entity<TextareaState>,
    comment_popup: Option<(CommentTarget, Point<Pixels>)>,
    comment_selection: Option<ReadPart>,
    open_comments: HashSet<String>,
    /// 会話ごと・エージェントごとの統計（モデル、料金、コンテキスト）
    stats: std::collections::BTreeMap<(String, String), fuzukue_proto::ServerEvent>,
    /// 探す（Ctrl+K）の入力欄。開いているときだけ
    palette: Option<Entity<InputState>>,
    palette_graph: Option<(String, Vec<fuzukue_proto::DeckRoute>)>,
    palette_search: Option<Task<()>>,
    /// PDF が開いたら移動するページ（0 始まり）
    pdf_goto: Option<usize>,
    /// 逆引きや保存の結果の一言
    notice: Option<SharedString>,
    link_tx: futures::channel::mpsc::UnboundedSender<MarkdownLink>,
}

#[derive(Clone)]
struct CommentTarget {
    card: String,
    quote: Option<String>,
    source: Option<CommentSource>,
    proof: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ReadPart {
    card: String,
    proof: bool,
}

struct ReadText {
    display: std::sync::Arc<TexMarkdown>,
    body: std::sync::Arc<str>,
    view: Entity<TextViewState>,
    uses_tex: bool,
}

struct PaperJump {
    card: String,
    tail: Pixels,
    pending: bool,
}

enum MarkdownLink {
    Card(String),
    File(String),
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn until_text(at: i64) -> String {
    let minutes = fuzukue_view::minutes_until(at, unix_now());
    if minutes >= 60 {
        tr!("workbench-time-hours-minutes", hours = minutes / 60, minutes = minutes % 60)
    } else {
        tr!("workbench-time-minutes", minutes = minutes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RightPane {
    /// 何も開いていない（チャットだけ）
    None,
    /// 研究の概要（カード一覧・Issue・出力）
    Overview,
    DeckCards,
    Exploration,
    Findings,
    Inbox,
    Card,
    Pdf,
    Issue(u64),
    ResearchIssues,
    BranchReview,
    Handoff,
    ResearchIssue(u64),
    /// 論文編集: カードを structure.yaml の順に並べ、コメントをためて送る
    Read,
    /// 計画や研究ノートの Markdown
    Note,
    Source,
    Files,
    Tests,
    Library,
    Extension,
    Trash,
    DeckComments,
}

/// 通して読む画面の 1 行。
#[derive(PartialEq, Eq)]
enum ReadRow {
    Title(String),
    Section(String),
    Subsection(String),
    Card(String),
}

/// 探す（Ctrl+K）で選んだものを開く方法。
#[derive(Clone)]
enum PaletteItem {
    Card(String),
    Source(String),
    Pdf(String),
    Tab(String),
    Deck(String),
    Thread(String),
    Settings,
    Screenshot,
}

fn wire_deck(path: &str) -> &str {
    if path == "." { "" } else { path }
}

fn thread_title(thread: &fuzukue_proto::ThreadView) -> &str {
    if thread.untitled || thread.title.is_empty() {
        tr!("threads-new-thread")
    } else {
        &thread.title
    }
}

fn deck_kind_label(kind: &str) -> &str {
    match kind {
        "paper" => tr!("workbench-paper"),
        "research" => tr!("workbench-research"),
        "book" => tr!("workbench-book"),
        "slides" => tr!("workbench-slides"),
        "document" => tr!("workbench-document"),
        "" => tr!("workbench-deck"),
        kind => kind,
    }
}

/// 小さなボタン。
fn pill(id: impl Into<SharedString>, label: impl Into<SharedString>, primary: bool, cx: &gpui_kit::App) -> gpui_kit::Stateful<gpui_kit::Div> {
    pill_content(id, label.into(), primary, cx)
}

fn icon_label(icon: Symbol, label: impl Into<SharedString>, color: gpui_kit::Hsla) -> gpui_kit::Div {
    h_flex().gap_2().child(icon.tinted(color)).child(label.into())
}

fn icon_pill(id: impl Into<SharedString>, icon: Symbol, label: impl Into<SharedString>, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    pill_content(id, icon_label(icon, label, cx.theme().muted_foreground), false, cx)
}

fn pill_content(id: impl Into<SharedString>, content: impl IntoElement, primary: bool, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    div()
        .id(gpui_kit::ElementId::Name(id.into()))
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .border_1()
        .border_color(theme.border)
        .when(primary, |d| d.bg(theme.primary).text_color(theme.primary_foreground))
        .hover(|d| d.opacity(0.85))
        .child(content)
}

fn composing<I: EntityInputHandler>(input: &Entity<I>, window: &mut Window, cx: &mut App) -> bool {
    input.update(cx, |input, cx| input.marked_text_range(window, cx).is_some())
}

fn focused_composition<I: Focusable + EntityInputHandler>(
    input: &Entity<I>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    input.read(cx).focus_handle(cx).is_focused(window) && composing(input, window, cx)
}

/// Handle navigation only in the dispatch subtree of a scroll viewport.
fn scroll_list_by_key(
    list: &gpui_kit::ListState,
    event: &gpui_kit::KeyDownEvent,
    window: &Window,
) -> bool {
    let modifiers = event.keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return false;
    }
    let line = window.line_height();
    let page = list.viewport_bounds().size.height.max(line);
    match (event.keystroke.key.as_str(), modifiers.shift) {
        ("up", false) => list.scroll_by(-line),
        ("down", false) => list.scroll_by(line),
        ("pageup", false) | ("space", true) => list.scroll_by(-page),
        ("pagedown" | "space", false) => list.scroll_by(page),
        ("home", false) => list.scroll_to(gpui_kit::ListOffset::default()),
        ("end", false) => list.scroll_to_end(),
        _ => return false,
    }
    true
}

impl Workbench {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Appearance::track_main_window(window, cx);
        Self::new_with_target(window, cx, None)
    }

    fn new_with_target(
        window: &mut Window,
        cx: &mut Context<Self>,
        target: Option<Result<conn::Target, &'static str>>,
    ) -> Self {
        let read_list = paper_list();
        let reader = cx.weak_entity();
        read_list.set_item_measurement_readiness(move |index, cx| {
            reader.upgrade().is_none_or(|view| view.read(cx).read_row_measurement_ready(index, cx))
        });
        let view_focus = cx.focus_handle();
        Self::track_window_close(window, cx);
        cx.on_focus_lost(window, |this, window, cx| {
            // A virtualized Markdown row can keep its focus handle after its
            // dispatch node is gone. Continue handling keys on the transcript.
            if window.focused(cx).is_some()
                && window.focus_lost_restore_target(cx).as_ref() == Some(&this.chat_focus)
            {
                this.chat_focus.focus(window, cx);
            }
        })
        .detach();
        let root_focus = view_focus.clone();
        let view = cx.weak_entity();
        let window_id = window.window_handle().window_id();
        // A view switch can remove the focused input. In that frame there is
        // no focused descendant to carry Escape up to the Workbench.
        let close_key = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            let mods = event.keystroke.modifiers;
            if !window.has_active_dialog(cx)
                && view.update(cx, |this, cx| {
                    this.handle_app_menu_key(&event.keystroke, window, cx)
                }).unwrap_or(false)
            {
                cx.stop_propagation();
            } else if !window.has_active_dialog(cx)
                && event.keystroke.key == "escape" && mods.number_of_modifiers() == 0
                && view.update(cx, |this, cx| this.dismiss_chat_quote(window, cx)).unwrap_or(false)
            {
                cx.stop_propagation();
            } else if !window.has_active_dialog(cx)
                && event.keystroke.key.eq_ignore_ascii_case("q") && mods.control && mods.shift
                && mods.number_of_modifiers() == 2
                && view.update(cx, |this, cx| this.quote_reply(window, cx)).unwrap_or(false)
            {
                cx.stop_propagation();
            } else if !window.has_active_dialog(cx) && view.update(cx, |this, cx| {
                if this.workspace_browser_open() {
                    return false;
                }
                if this.file_browser_supported()
                    && event.keystroke.key.eq_ignore_ascii_case("o")
                    && mods.number_of_modifiers() == 1 && (mods.control || mods.secondary(cx))
                {
                    this.toggle_file_browser(window, cx);
                    true
                } else {
                    this.file_picker_key(&event.keystroke, window, cx)
                }
            }).unwrap_or(false) {
                cx.stop_propagation();
            } else if mods.control && mods.shift && !mods.alt && !mods.platform
                && event.keystroke.key.eq_ignore_ascii_case("s")
            {
                let _ = view.update(cx, |this, cx| this.capture_screenshot(window, cx));
                cx.stop_propagation();
            } else if event.keystroke.key == "escape"
                && !root_focus.contains_focused(window, cx) && !window.has_active_dialog(cx)
            {
                let _ = view.update(cx, |this, cx| {
                    if this.close_on_escape(window, cx) {
                        cx.stop_propagation();
                    }
                });
            }
        });
        let appearance = Appearance::new(window, cx);
        let pane_resize = pane_resize::PaneResize::load(appearance.settings_path(), cx);
        let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
        let shot_managed = target.is_some();
        let agent_models = agent_models::ModelSettings::load(
            appearance.settings_path().filter(|_| !shot_managed),
        );
        let notifications = notifications::Notifications::new(appearance.settings_path(), shot_managed, window, cx);
        let distribution = distribution::Distribution::new(shot_managed, appearance.send_queue_path());
        let (out, connection) = conn::spawn(
            target.unwrap_or_else(|| appearance.server_target()),
            ev_tx,
            if shot_managed { None } else { appearance.settings_path() },
            #[cfg(all(target_os = "linux", feature = "headless-shot"))]
            shot_managed,
        );
        connection.track_server(cx);
        cx.on_app_quit(|this, cx| {
            this.connection.shutdown();
            conn::Connection::close_parked_servers(cx);
            if let Some(handoff) = this.distribution.handoff.take() {
                if let Err(err) = handoff.release() {
                    eprintln!("{}", tr!("distribution-update-error", detail = format!("{err:#}")));
                }
            }
            std::future::ready(())
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = ev_rx.next().await {
                if this.update_in(cx, |this, window, cx| this.on_conn(ev, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_secs(12)).await;
            if this.update(cx, |this, _| this.github_view()).is_err() {
                break;
            }
        }).detach();
        let (link_tx, mut link_rx) = futures::channel::mpsc::unbounded::<MarkdownLink>();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(link) = link_rx.next().await {
                let result = this.update_in(cx, |this, window, cx| match link {
                    MarkdownLink::Card(id) => this.jump_to_card(id, cx),
                    MarkdownLink::File(target) => {
                        if let Some((path, lines)) = fuzukue_view::decode_file_link(&target) {
                            this.open_source_file(path.into_owned(), lines, window, cx);
                        }
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();

        let comment_input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6).placeholder(tr!("workbench-write-a-comment")));
        cx.subscribe_in(&comment_input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: true, shift: false } = ev {
                this.add_comment(window, cx);
            }
        })
        .detach();

        let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 8).placeholder(tr!("workbench-enter-a-message")));
        composer_markdown::observe(&input, window, cx);
        let composer_focus = input.read(cx).focus_handle(cx);
        cx.on_focus(&composer_focus, window, |_, window, cx| TextSelection::clear(window, cx)).detach();
        cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: true, shift: false } = ev {
                this.send_prompt(window, cx);
            } else if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();

        let mut me = Self {
            #[cfg(all(target_os = "linux", feature = "headless-shot"))]
            shot_activity: None,
            appearance,
            notifications,
            approvals: Default::default(),
            settings_open: false,
            trash_items: None,
            trash_error: None,
            extensions: Default::default(),
            sidebar: Default::default(),
            thread_info: Default::default(),
            distribution,
            self_update: Default::default(),
            screenshots: Default::default(),
            history: Default::default(),
            branch_review: Default::default(),
            handoffs: Default::default(),
            chats: HashMap::new(),
            workspace_requests: Default::default(),
            recent_transcripts: VecDeque::new(),
            question_drafts: HashMap::new(),
            chat_capabilities: None,
            codex_models: Vec::new(),
            delegate_settings: None,
            worker_approval_policy: None,
            agent_settings: Default::default(),
            agent_management: Default::default(),
            agent_models,
            review_settings: None,
            partner_default: None,
            active: String::new(),
            github: Default::default(),
            github_report: Default::default(),
            issues: Vec::new(),
            repo: String::new(),
            issue_error: None,
            deck_overview: Default::default(),
            workshop: Default::default(),
            exploration: Default::default(),
            input,
            composer_markdown: Default::default(),
            out,
            workspaces: Default::default(),
            receiving_backlog: false,
            transcripts: Default::default(),
            connection,
            status: ConnectionStatus::Connecting,
            cards: Vec::new(),
            card_diags: Vec::new(),
            lazy_deck: Default::default(),
            style_note_disclosure: Default::default(),
            // 動作確認用: 起動時に選んでおくカード。
            selected: fuzukue_proto::compat::env("FUZUKUE_SELECT").ok(),
            pdfs: Vec::new(),
            right: RightPane::None,
            reader_selection_scope: TextSelectionScopeId::new(),
            pane_resize,
            pdf: None,
            pdf_status: None,
            pdf_path: None,
            pdf_request: 0,
            pdf_digest: None,
            http: HttpTarget::default(),
            connection_epoch: 0,
            usage: Vec::new(),
            pause_at: 0.95,
            from_pdf: false,
            tabs: Vec::new(),
            workspace_title: None,
            threads: Vec::new(),
            threads_loaded: false,
            thread_title_edit: None,
            auto_prompt: fuzukue_proto::compat::env("FUZUKUE_AUTOPROMPT").ok(),
            pending_research_origin: None,
            active_tab: None,
            deck: String::new(),
            note: None,
            source: SourcePane::default(),
            files: Default::default(),
            tests: Default::default(),
            symbols: Default::default(),
            library: Default::default(),
            pdf_goto: None,
            lean: HashMap::new(),
            lean_running: false,
            lean_doc_findings: Vec::new(),
            comments: Vec::new(),
            comment_target: None,
            comment_input,
            comment_popup: None,
            comment_selection: None,
            open_comments: HashSet::new(),
            palette: None,
            palette_graph: None,
            palette_search: None,
            stats: std::collections::BTreeMap::new(),
            doc_title: String::new(),
            doc_sections: Vec::new(),
            document_deck: None,
            doc_macros: Vec::new(),
            doc_math_macros: Vec::new(),
            doc_references: None,
            read_rows: Vec::new(),
            read_md: HashMap::new(),
            read_list,
            open_proofs: HashMap::new(),
            proof_comments: HashSet::new(),
            paper_jump: None,
            reviews: Default::default(),
            findings: Default::default(),
            pending_paper_open: None,
            paper_highlight: None,
            card_editor: None,
            card_tex_build: None,
            card_save_request: 0,
            translation_save_request: None,
            paper_jobs: Default::default(),
            paper_job_result: None,
            pdf_audit: Vec::new(),
            review_jobs: Default::default(),
            review_clock: None,
            chat_activity_clock: None,
            chat_waiting: Default::default(),
            paper_focus: cx.focus_handle(),
            chat_focus: cx.focus_handle(),
            view_focus,
            app_menu: menu_bar::MenuBar::new(cx),
            _close_key: close_key,
            built_pdfs: HashMap::new(),
            paper_pdf: Default::default(),
            protocol: 0,
            safe_close: Default::default(),
            editing: None,
            notice: None,
            link_tx,
        };
        let workbench = cx.entity().downgrade();
        me.read_list.set_item_measurement_readiness(move |index, cx| {
            workbench.upgrade().is_none_or(|entity| entity.read(cx).paper_row_ready(index, cx))
        });
        me.chat("", cx);
        me.distribution_timer(cx);
        me
    }

    /// 会話の表示状態（なければ作る）。
    fn chat(&mut self, session: &str, cx: &mut Context<Self>) -> &mut Chat {
        if !self.chats.contains_key(session) {
            let mut chat = Chat::new(cx);
            if let Some(settings) = self.threads.iter().find(|thread| thread.id == session)
                .and_then(|thread| thread.partner_settings.as_ref())
            {
                chat.settings.clone_from(settings);
            }
            self.chats.insert(session.to_string(), chat);
        }
        self.chats.get_mut(session).unwrap()
    }

    fn display_note_markdown(&self, body: &str) -> String {
        paper_markdown(&expand_macros(body, &self.doc_macros), self.doc_references.as_ref())
    }

    fn display_card_tex(&self, body: &str) -> TexMarkdown {
        read_card_tex(body, self.doc_references.as_ref(), &self.doc_macros)
    }

    fn markdown_link_handler(
        &self,
    ) -> impl Fn(&SharedString, &gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App)
           + Send
           + Sync
           + 'static {
        let tx = self.link_tx.clone();
        move |url, _, _, cx| {
            let link = if let Some(id) = url.strip_prefix("fuzukue-card:").or_else(|| url.strip_prefix("labo-card:")) {
                MarkdownLink::Card(id.to_owned())
            } else if let Some(target) = url.strip_prefix("fuzukue-file:").or_else(|| url.strip_prefix("labo-file:")) {
                MarkdownLink::File(target.to_owned())
            } else {
                cx.open_url(url);
                return;
            };
            let _ = tx.unbounded_send(link);
        }
    }

    fn add_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.comment_target.clone() else {
            return;
        };
        if composing(&self.comment_input, window, cx) {
            return;
        }
        let text = self.comment_input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        if self
            .out
            .send(ClientMsg::AddComment {
                deck: self.deck.clone(),
                card: target.card.clone(),
                text,
                proof: target.quote.as_ref().map(|_| target.proof),
                quote: target.quote,
                source: target.source,
            })
            .is_ok()
        {
            self.open_comments.insert(target.card);
            self.comment_target = None;
            self.comment_input
                .update(cx, |input, cx| input.clean(window, cx));
            cx.notify();
        }
    }

    fn start_comment(
        &mut self,
        target: CommentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.comment_target.is_none() {
            self.comment_input
                .update(cx, |input, cx| input.clean(window, cx));
        }
        self.comment_target = Some(target);
        self.comment_popup = None;
        TextSelection::clear(window, cx);
        self.comment_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    fn finish_comment_selection(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(part) = self.comment_selection.take() else {
            return;
        };
        let entity = cx.entity().downgrade();
        // 選択を確定するマウスイベントの処理が終わってから読む。
        window.defer(cx, move |window, cx| {
            // コピーの投影で、次の描画を待たずに各カードの選択範囲を確定する。
            if TextSelection::selected_text(window, cx).is_empty() {
                return;
            }
            let _ = entity.update(cx, |this, cx| {
                let Some(text) = this.read_md.get(&part) else {
                    return;
                };
                let state = text.view.read(cx);
                let quote = state.selected_text().trim().to_owned();
                if quote.is_empty() {
                    return;
                }
                let source = state.selected_source_range()
                    .and_then(|range| text.display.tex_range(range))
                    .and_then(|range| {
                        let card = this.cards.iter().find(|card| card.id == part.card)?;
                        let text = card.body.get(range.clone())?.to_owned();
                        Some(CommentSource { range, text })
                    });
                let bounds = state.bounds();
                let mut offset = position - bounds.origin;
                offset.x = offset.x.clamp(px(0.), (bounds.size.width - px(84.)).max(px(0.)));
                offset.y = (offset.y - px(34.)).clamp(px(0.), (bounds.size.height - px(28.)).max(px(0.)));
                this.comment_popup = Some((CommentTarget { card: part.card, proof: part.proof, quote: Some(quote), source }, offset));
                cx.notify();
            });
        });
    }

    fn composer(
        input: &Entity<TextareaState>,
        id: &'static str,
        label: &'static str,
        placeholder: &'static str,
        submit: fn(&mut Self, &mut Window, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> gpui_kit::Div {
        let theme = cx.theme();
        v_flex()
            .w_full()
            .flex_none()
            .p_3()
            .gap_1()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                Textarea::new(input)
                    .appearance(false)
                    .aria_label(placeholder)
                    .text_base()
                    .line_height(relative(1.5)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .px_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(tr!("workbench-press-ctrl-enter-to-send")),
                    )
                    .child(pill(id, label, true, cx).on_click(cx.listener(
                        move |this, _, window, cx| {
                            submit(this, window, cx);
                        },
                    ))),
            )
    }

    /// デッキを開く。研究では直近の添付スレッド、本・原稿では現在の会話を表示する。
    fn select_tab(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else { return };
        let session = if matches!(tab.kind.as_str(), "paper" | "book") {
            None
        } else {
            Self::tab_attachment(tab)
                .and_then(|deck| self.threads.iter().find(|thread| thread.attached.iter().any(|path| path == deck)))
                .map(|thread| thread.id.clone())
        };
        self.open_tab(id, cx);
        if let Some(session) = session {
            self.select_thread(session, cx);
        }
    }

    fn open_tab(&mut self, id: String, cx: &mut Context<Self>) {
        self.close_reviews();
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else { return };
        self.deck_overview.clear_deck_request();
        self.settings_open = false;
        self.active_tab = Some(id);
        self.right = match tab.kind.as_str() {
            "paper" | "book" => RightPane::Read,
            _ => RightPane::Overview,
        };
        let deck = tab.deck.as_ref().or_else(|| tab.papers.iter().flat_map(|paper| &paper.decks).next());
        if let Some(deck) = deck.filter(|deck| deck.as_str() != self.deck) {
            let _ = self.out.send(ClientMsg::SelectDeck { deck: deck.clone() });
        }
        cx.notify();
    }

    fn paper_deck(&self, deck: &str) -> bool {
        self.tabs.iter().any(|tab| {
            (matches!(tab.kind.as_str(), "paper" | "book") && tab.deck.as_deref() == Some(deck))
                || tab.papers.iter().any(|paper| paper.decks.iter().any(|path| path == deck))
        })
    }

    fn open_paper_deck(&mut self, deck: String, cx: &mut Context<Self>) {
        self.close_reviews();
        self.files.paper_file = None;
        self.pending_paper_open = None;
        self.settings_open = false;
        self.active_tab = self.tabs.iter()
            .find(|tab| tab.deck.as_deref() == Some(&deck)).map(|tab| tab.id.clone());
        self.right = RightPane::Read;
        if self.deck != deck {
            let _ = self.out.send(ClientMsg::SelectDeck { deck });
        }
        cx.notify();
    }

    fn open_attachment(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Some((workspace, deck)) = path.split_once(':').filter(|(workspace, _)| {
            self.workspaces.entries.iter().any(|entry| &entry.id == workspace)
        }) {
            self.navigate_workshop(workspace.into(), workshop::Target::Deck(deck.into()), cx);
            return;
        }
        let deck = wire_deck(path);
        if self.paper_deck(deck) {
            self.open_paper_deck(deck.to_owned(), cx);
        } else if let Some(tab) = self.tabs.iter().find(|tab| tab.deck.as_deref() == Some(deck)) {
            self.open_tab(tab.id.clone(), cx);
        } else {
            self.settings_open = false;
            self.active_tab = None;
            self.right = RightPane::Overview;
            let _ = self.out.send(ClientMsg::SelectDeck { deck: deck.to_owned() });
            cx.notify();
        }
    }

    /// 計画、ノート、参照先のテキストファイルを右ペインに開く。
    fn open_note(&mut self, path: String, cx: &mut Context<Self>) {
        let epoch = self.connection_epoch;
        let http = self.http.clone();
        self.right = RightPane::Note;
        self.note = Some((path.clone(), tr!("workbench-loading").into()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let got = cx.background_executor().spawn(async move { http.fetch(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch || this.note.as_ref().is_none_or(|(p, _)| p != &path) {
                    return;
                }
                let text = match got {
                    Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                    Err(e) => tr!("workbench-cannot-read", e = e.to_string()),
                };
                // frontmatter は見出しとして出さない
                let body = if path.ends_with(".md") {
                    fuzukue_cards::split_frontmatter(&text).map(|(_, body)| body.to_owned()).unwrap_or(text)
                } else {
                    text
                };
                this.note = Some((path, body));
                cx.notify();
            });
        })
        .detach();
    }

    fn render_note_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pane = v_flex().size_full().min_w_0().overflow_hidden();
        let Some((path, body)) = &self.note else { return pane };
        let markdown = if path.ends_with(".md") {
            self.display_note_markdown(body)
        } else {
            body.lines().map(|line| format!("    {line}\n")).collect()
        };
        pane.child(self.pane_heading(
            div().truncate().text_xs().text_color(theme.muted_foreground).child(path.clone()), cx,
        ).px(PANE_INSET).py_1()).child(
            div().id("note-pane").flex_1().min_h_0().w_full().overflow_y_scrollbar().child(
                self.appearance.style_reading(div().w_full().px(PANE_INSET).py_4()).child(
                    TextView::markdown(SharedString::from(format!("note-{path}")), markdown)
                        .markdown_extensions(math::extensions())
                        .selectable(true)
                        .on_link_click(self.markdown_link_handler()),
                ),
            ),
        )
    }

    /// 探す（Ctrl+K）の候補。題の一致、グラフの一致、最終活動の順で並べる。
    fn palette_matches(&self, query: &str) -> Vec<(String, PaletteItem)> {
        let q = query.trim().to_lowercase();
        let title_score = |title: &str| {
            let title = title.to_lowercase();
            if q.is_empty() { 0 }
            else if title == q { 100 }
            else if title.starts_with(&q) { 85 }
            else if title.contains(&q) { 70 }
            else if q.split_whitespace().all(|term| title.contains(term)) { 60 }
            else { 0 }
        };
        let hit = |text: &str| q.is_empty() || title_score(text) > 0;
        let graph = self.palette_graph.as_ref()
            .filter(|(searched, _)| *searched == q)
            .map(|(_, decks)| decks.as_slice()).unwrap_or_default();
        if self.library.move_target.is_some() {
            let mut seen = HashSet::new();
            return graph.iter().map(|deck| (deck.deck.clone(), deck.title.clone()))
                .chain(self.library.decks.iter().filter(|deck| hit(deck) || hit(&self.deck_title(deck)))
                    .map(|deck| (deck.clone(), self.deck_title(deck))))
                .filter(|(deck, _)| deck != "memos" && seen.insert(deck.clone()))
                .take(14)
                .map(|(deck, title)| (title, PaletteItem::Deck(deck)))
                .collect();
        }
        let graph_score = |path: &str| graph.iter().position(|deck| deck.deck == path)
            .map_or(0, |rank| 50 - rank.min(49));
        let mut recency = HashMap::new();
        let mut ranked = Vec::new();
        for thread in self.threads.iter().filter(|thread| !thread.archived) {
            for path in &thread.attached {
                let latest = recency.entry(path.as_str()).or_insert(0);
                *latest = (*latest).max(thread.updated);
            }
            let linked = thread.attached.iter().map(|path| graph_score(path)).max().unwrap_or(0);
            let score = title_score(thread_title(thread)).max(linked / 2);
            if q.is_empty() || score > 0 {
                ranked.push((score, thread.updated, tr!("workbench-thread-title", title = thread_title(thread)), PaletteItem::Thread(thread.id.clone())));
            }
        }
        let mut seen = HashSet::new();
        let mut add_deck = |path: &str, title: &str, kind: &str, item: PaletteItem| {
            let path = if path.is_empty() { "." } else { path };
            if !seen.insert(path.to_owned()) {
                return;
            }
            let indexed = graph.iter().find(|deck| deck.deck == path);
            let indexed_title = indexed.map_or(title, |deck| deck.title.as_str());
            let kind = indexed.map(|deck| deck.kind.as_str()).filter(|kind| !kind.is_empty()).unwrap_or(kind);
            let label = deck_kind_label(kind);
            let score = title_score(title).max(title_score(indexed_title))
                .max(if hit(path) && !q.is_empty() { 55 } else { 0 })
                .max(graph_score(path));
            if q.is_empty() || score > 0 {
                ranked.push((score, recency.get(path).copied().unwrap_or(0),
                    format!("{label} · {indexed_title}"), item));
            }
        };
        for tab in &self.tabs {
            if let Some(path) = &tab.deck {
                add_deck(path, &tab.title, &tab.kind, PaletteItem::Tab(tab.id.clone()));
            } else if hit(&tab.title) || hit(&tab.id) {
                add_deck(&tab.id, &tab.title, &tab.kind, PaletteItem::Tab(tab.id.clone()));
            }
        }
        for tab in &self.tabs {
            for paper in &tab.papers {
                for deck in &paper.decks {
                    add_deck(deck, &paper.name, "paper", PaletteItem::Deck(deck.clone()));
                }
            }
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let mut out: Vec<_> = ranked.into_iter().map(|(_, _, label, item)| (label, item)).collect();
        for c in self.cards.iter().filter(|c| !c.external) {
            if hit(&c.id) || hit(&c.title) {
                let title = if c.title.is_empty() { c.id.clone() } else { format!("{} ({})", c.title, c.id) };
                out.push((tr!("workbench-card", title = title.to_string()), PaletteItem::Card(c.id.clone())));
            }
        }
        for p in &self.pdfs {
            if hit(p) {
                out.push((format!("PDF · {p}"), PaletteItem::Pdf(p.clone())));
            }
        }
        if let Some(path) = query.trim().strip_prefix("file:").map(str::trim).filter(|path| !path.is_empty()) {
            return vec![(tr!("workbench-open-file", path = path.to_string()), PaletteItem::Source(path.to_owned()))];
        }
        let settings = hit(tr!("workbench-appearance")) || hit("settings") || hit("appearance");
        out.truncate(if settings { 13 } else { 14 });
        if settings {
            let item = (tr!("workbench-settings-appearance").into(), PaletteItem::Settings);
            if q.is_empty() {
                out.push(item);
            } else {
                out.insert(0, item);
            }
        }
        let capture = tr!("screenshot-capture");
        if hit(capture) || hit("screenshot") {
            out.insert(0, (capture.into(), PaletteItem::Screenshot));
            out.truncate(14);
        }
        out
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_palette(None, window, cx);
    }

    fn open_memo_destination(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_palette(Some(id), window, cx);
    }

    fn show_palette(&mut self, memo: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette();
        self.library.move_target = memo;
        let placeholder = if self.library.move_target.is_some() {
            tr!("library-memo-search-destination")
        } else {
            tr!("workbench-search-decks-and-threads-file-relative-path")
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => {
                let q = input.read(cx).value().to_string();
                if let Some((_, item)) = this.palette_matches(&q).into_iter().next() {
                    this.palette_open(item, window, cx);
                }
            }
            InputEvent::Change => {
                this.palette_search = None;
                let query = input.read(cx).value().trim().to_lowercase();
                if this.protocol >= 13 && !query.is_empty() && query.chars().count() <= 250 {
                    let out = this.out.clone();
                    this.palette_search = Some(cx.spawn(async move |_, cx| {
                        cx.background_executor().timer(Duration::from_millis(120)).await;
                        let _ = out.send(ClientMsg::SearchDecks { query });
                    }));
                }
                cx.notify();
            }
            _ => {}
        })
        .detach();
        self.palette = Some(input);
        cx.notify();
    }

    fn close_palette(&mut self) {
        self.palette = None;
        self.palette_search = None;
        self.library.move_target = None;
    }

    fn palette_open(&mut self, item: PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        let memo = self.library.move_target.take();
        self.close_palette();
        if let (Some(id), PaletteItem::Deck(deck)) = (memo, &item) {
            self.move_memo(id, deck.clone(), cx);
            return;
        }
        if matches!(item, PaletteItem::Screenshot) {
            self.capture_screenshot(window, cx);
            return;
        }
        if matches!(item, PaletteItem::Settings) {
            self.toggle_settings(cx);
            return;
        }
        self.settings_open = false;
        self.agent_management.tab = Default::default();
        self.sidebar.server_settings = false;
        match item {
            PaletteItem::Source(path) => self.open_source_file(path, None, window, cx),
            PaletteItem::Card(id) => self.jump_to_card(id, cx),
            PaletteItem::Pdf(p) => self.open_artifact_pdf(p, Some(window), cx),
            PaletteItem::Tab(id) => self.select_tab(id, cx),
            PaletteItem::Deck(d) => self.open_paper_deck(d, cx),
            PaletteItem::Thread(id) => self.select_thread(id, cx),
            PaletteItem::Settings | PaletteItem::Screenshot => unreachable!(),
        }
        cx.notify();
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.palette.as_ref()?;
        let theme = cx.theme().clone();
        let query = input.read(cx).value().to_string();
        let mut list = v_flex().gap_0p5();
        for (i, (label, item)) in self.palette_matches(&query).into_iter().enumerate() {
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-{i}")))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(i == 0, |d| d.bg(theme.secondary))
                    .hover(|d| d.bg(theme.accent))
                    .child(label)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| this.palette_open(item.clone(), window, cx))),
            );
        }
        Some(
            div()
                .absolute()
                .top(px(48.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    v_flex()
                        .id("title-search")
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.close_palette();
                            cx.notify();
                            cx.stop_propagation();
                        }))
                        .w(px(560.))
                        .p_3()
                        .gap_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .shadow_lg()
                        .child(h_flex().justify_between().gap_2()
                            .child(div().text_xs().text_color(theme.muted_foreground).child(if self.library.move_target.is_some() {
                                tr!("library-memo-move-shortcut")
                            } else {
                                tr!("workbench-search-enter-to-open-esc-to-close")
                            }))
                            .child(self.pane_close_button("close-search", cx)))
                        .child(Input::new(input))
                        .child(list),
                )
                .into_any_element(),
        )
    }

    fn load_workspace_title(&self, cx: &mut Context<Self>) {
        let http = self.http.clone();
        let epoch = self.connection_epoch;
        cx.spawn(async move |this, cx| {
            let title = cx.background_executor().spawn(async move {
                let bytes = http.fetch_workspace_config().ok()?;
                let project: toml::Table = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
                project.get("title")?.as_str().map(str::trim)
                    .filter(|title| !title.is_empty()).map(str::to_owned)
            }).await;
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch == epoch {
                    this.workspace_title = title;
                    cx.notify();
                }
            });
        }).detach();
    }

    fn switch_session(&mut self, session: String, cx: &mut Context<Self>) {
        self.chat(&session, cx);
        if self.active != session {
            self.approvals.clear_controls();
            self.pending_paper_open = None;
            self.github.unread.remove(&self.active);
            if let Some(chat) = self.chats.get_mut(&self.active) {
                chat.clear_transcript_hover();
            }
            self.chats[&session].activate_transcript();
            self.recent_transcripts.retain(|id| id != &session);
            self.recent_transcripts.push_back(session.clone());
            if self.recent_transcripts.len() > 12 {
                let evicted = self.recent_transcripts.pop_front().unwrap();
                if let Some(chat) = self.chats.get(&evicted) {
                    chat.discard_transcript_layout();
                }
            }
        }
        if !self.transcripts.scoped && self.active != session {
            self.restore_approval_notices(&session, cx);
        }
        self.active = session;
        self.request_thread_tail();
        self.github_view();
        self.request_thread_artifacts();
        cx.notify();
    }

    fn on_conn(&mut self, ev: ConnEvent, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "linux", feature = "headless-shot"))]
        if let (Some(activity), ConnEvent::Event(env)) = (&mut self.shot_activity, &ev) {
            activity.record(&env.event);
        }
        match ev {
            ConnEvent::Connected => {
                self.receiving_backlog = true;
                self.safe_close_reconnected();
                if !matches!(self.status, ConnectionStatus::ServerSwap) {
                    self.status = ConnectionStatus::Connected;
                }
            }
            ConnEvent::ApprovalQueueAvailable(available) => self.approvals.available = available,
            ConnEvent::AdvertisedTranscriptCache(cached) => self.advertised_transcript_cache(cached),
            ConnEvent::OwnedServerStopped => self.owned_server_stopped(window, cx),
            ConnEvent::Disconnected(e) => {
                self.approvals.disconnect();
                self.workspace_requests = Default::default();
                self.agent_models.disconnect();
                self.chat_waiting = Default::default();
                for chat in self.chats.values_mut() {
                    chat.set_consulting(false);
                }
                self.disconnect_transcripts(cx);
                self.screenshot_disconnected();
                self.deck_overview.disconnected();
                self.github_disconnected(cx);
                self.github_report_disconnected(cx);
                self.disconnect_chat_uploads();
                self.symbols.clear();
                self.library.disconnect();
                self.clear_extensions(window, cx);
                self.disconnect_card_edit(cx);
                if self.translation_save_request.take().is_some() {
                    self.notice = Some(tr!("paper-connection-lost-before-the-save-result-arrived").into());
                }
                self.invalidate_review_findings();
                self.card_file_disconnected(cx);
                self.source_disconnected(cx);
                self.thread_info.git.disconnect();
                self.history.reset();
                self.branch_review.reset();
                self.handoffs = Default::default();
                self.tests.disconnect();
                self.workspace_browser_disconnected();
                self.file_browser_disconnected(window, cx);
                self.pending_paper_open = None;
                self.thread_info.compaction.retain(|_, (state, _)| state != "running");
                if !matches!(self.status, ConnectionStatus::ServerSwap) {
                    self.status = ConnectionStatus::Reconnecting(e);
                }
            }
            ConnEvent::Status(message) => self.status = ConnectionStatus::Detail(message),
            ConnEvent::Reset(http) => {
                self.notifications.reset_connection();
                self.approvals = Default::default();
                self.question_drafts.clear();
                self.workspaces = Default::default();
                self.out.select_workspace(None);
                self.protocol = 0;
                self.transcripts.scoped = false;
                if self.connection_epoch != 0 {
                    self.clear_workspace(window, cx);
                }
                self.connection_epoch += 1;
                self.http = http;
                self.distribution.reset_connection();
                self.self_update = Default::default();
                self.reset_safe_close(window, cx);
                self.check_client_update(cx);
                self.status = ConnectionStatus::Connecting;
            }
            ConnEvent::Event(env) => {
                let notification = matches!(env.event, ServerEvent::Notify { .. });
                if notification || env.workspace.as_deref().is_none_or(|id| self.workspaces.active.as_deref() == Some(id)) {
                    let advance = self.transcripts.scoped && !self.receiving_backlog && env.seq != 0;
                    match env.event {
                        ServerEvent::Notify { session, kind, title } if !self.receiving_backlog => {
                            if let Some(workspace) = &env.workspace {
                                self.notify_thread(workspace.clone(), session, kind, title, window, cx);
                            }
                        }
                        ServerEvent::SelfUpdateStatus { status } => {
                            self.notify_update(&status, window, cx);
                            self.update_self_update(status, cx);
                        }
                        ServerEvent::ShutdownWork { work } => self.shutdown_work(work),
                        ServerEvent::DrainStatus { status } => self.drain_progress(status, window, cx),
                        ServerEvent::ShutdownError { message } => self.safe_close_error(message, window, cx),
                        ServerEvent::ServerSwapNotice => {
                            self.connection.reap_stopping_server(&self.http, cx);
                            self.status = ConnectionStatus::ServerSwap;
                        }
                        ServerEvent::ThreadHistoryStart { session, before, workers } if env.workspace.is_some() => {
                            self.start_thread_history(&session, before, workers, cx);
                        }
                        event @ ServerEvent::ThreadHistoryPage { .. } if env.workspace.is_some() => {
                            self.finish_thread_history(event, cx);
                        }
                        event @ ServerEvent::ThreadHistoryFrom { .. } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.finish_thread_history_from(event, cx);
                        }
                        event @ ServerEvent::ThreadJobDetails { .. } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.finish_thread_job(event, cx);
                        }
                        ServerEvent::ThreadJobs { session, through, first, jobs } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.update_thread_jobs(&session, through, first, jobs, cx);
                        }
                        ServerEvent::ThreadJobUpdated { session, through, summary } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.update_thread_job(&session, through, summary, cx);
                        }
                        ServerEvent::ThreadJobReport { session, job, request, result } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.finish_job_report(&session, &job, request, result, window, cx);
                        }
                        ServerEvent::ThreadHistoryFailed { session, request, reason } if env.workspace.is_some() => {
                            self.thread_history_failed(&session, request, reason, cx);
                        }
                        event @ ServerEvent::ThreadTail { .. } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.finish_thread_tail(event, cx);
                        }
                        ServerEvent::ThreadSync { epoch, through, reset_sessions }
                            if self.transcripts.scoped && self.receiving_backlog =>
                        {
                            if let Some(workspace) = &env.workspace {
                                self.sync_thread_transcripts(workspace, epoch, through, reset_sessions, cx);
                                self.receiving_backlog = false;
                                self.finish_notification_route(cx);
                                self.request_thread_tail();
                            }
                        }
                        ServerEvent::ThreadHistoryInvalidated { session } if self.transcripts.scoped && env.workspace.is_some() => {
                            self.reset_thread_history(&session, cx);
                            if self.active == session {
                                self.request_thread_tail();
                            }
                        }
                        ServerEvent::ClientScreenshotRequested { id } => self.screenshot_requested(id, window, cx),
                        ServerEvent::IssueIntakeMove { session, repo, from, title } if !self.receiving_backlog => {
                            self.confirm_intake_move(fuzukue_proto::IssueThreadAction::SetIntake {
                                session, repo, move_from: Some(from),
                            }, title, window, cx);
                        }
                        ServerEvent::ScreenshotSaved { request, path, error } => self.screenshot_saved(request, path, error),
                        ServerEvent::WorkspaceSelected { id, replay } => {
                            if replay || self.workspaces.active.as_deref() != Some(id.as_str()) {
                                self.receiving_backlog = true;
                            }
                            let changed = self.workspaces.active.as_ref() != Some(&id);
                            self.workspace_selected(id, replay, window, cx);
                            if changed || !self.approvals.ready() {
                                self.approvals.clear_controls();
                                self.refresh_approvals();
                            }
                        }
                        ServerEvent::ApprovalQueue { request, result } => self.approvals.snapshot(request, result),
                        ServerEvent::ApprovalQueueChanged { approval } => self.approval_changed(approval, false, window, cx),
                        ServerEvent::ApprovalDecisionResult { request, result } => self.approval_result(request, result, window, cx),
                        ServerEvent::Workspaces { workspaces } => self.workspaces.entries = workspaces,
                        ServerEvent::WorkspaceDirectory { request, result } => self.workspace_directory(request, result, window, cx),
                        ServerEvent::FileList { request, result } => self.on_file_list(request, result),
                        ServerEvent::FileMatches { request, result } => self.on_file_matches(request, result),
                        ServerEvent::FileResolved { request, result } => self.on_file_resolved(request, result, window, cx),
                        ServerEvent::WorkspaceCreated { request, result } => self.workspace_created(request, result, window, cx),
                        ServerEvent::WorkspaceError { message } => {
                            self.cancel_notification_route();
                            self.workspace_error(message);
                        }
                        event @ (ServerEvent::Agents { .. } | ServerEvent::AgentSettingsChanged { .. }) => {
                            self.on_event(event, cx);
                            self.sync_agent_hints(window, cx);
                        }
                        ServerEvent::Extensions { items } => self.extension_items(items),
                        ServerEvent::ExtensionPermissionPrompt { item } => self.extension_prompt(item, true, window, cx),
                        ServerEvent::ExtensionView { id, request, result } => self.extension_view(id, request, result, window, cx),
                        ServerEvent::ExtensionsChanged => { let _ = self.out.send(ClientMsg::ListExtensions); }
                        ServerEvent::ExtensionError { message } => self.extensions.error = Some(message),
                        ServerEvent::ExtensionUpdated { id, changed_files } => self.extensions.update = Some((id, changed_files)),
                        ServerEvent::LspDocument { document, update } => self.on_lsp_document(document, update, cx),
                        ServerEvent::LspResult { request, outcome } => self.on_lsp_result(request, outcome),
                        ServerEvent::SourceSaveResult { request, outcome } => {
                            if self.card_file_save_pending(request) {
                                self.on_card_file_saved(request, outcome, cx);
                            } else {
                                self.on_source_saved(request, outcome, window, cx);
                            }
                        }
                        ServerEvent::MemoResult { request, error } => self.on_memo_result(request, error, window, cx),
                        ServerEvent::LedgerResult { request, view } => self.on_ledger_result(request, view, cx),
                        ServerEvent::LedgerChanged => self.ledger_changed(cx),
                        ServerEvent::LibraryResults { request, hits, more, decks } => self.on_library_results(request, hits, more, decks),
                        ServerEvent::SymbolResults { request, results } => self.on_symbol_results(request, results, window, cx),
                        event @ ServerEvent::Threads { .. } => {
                            if !self.transcripts.scoped {
                                // Legacy snapshots mark the end of replay.
                                self.receiving_backlog = false;
                                self.on_event(event, cx);
                                self.restore_replay_positions();
                                self.finish_notification_route(cx);
                            } else if self.scoped_event(env.seq, &event, cx).is_some() {
                                let selection_was_listed = self.threads.iter().any(|thread| thread.id == self.active);
                                self.on_event(event, cx);
                                if !selection_was_listed {
                                    self.request_thread_tail();
                                }
                            }
                        }
                        ServerEvent::OpenFile { session, path, lines }
                            if !self.receiving_backlog && session == self.active =>
                        {
                            let lines = lines.map(|(start, end)| start as usize..=end as usize);
                            self.open_source_file(path, lines, window, cx);
                        }
                        event if self.transcripts.scoped && env.workspace.is_some() => {
                            if let Some(render) = self.scoped_event(env.seq, &event, cx) {
                                self.apply_event_with_transcript(event, self.receiving_backlog, render, cx);
                            }
                        }
                        event => self.on_event(event, cx),
                    }
                    if advance {
                        if let Some(workspace) = &env.workspace {
                            self.out.applied_transcript_seq(workspace, env.seq);
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    fn clear_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_scoped_workspace();
        self.screenshots = Default::default();
        self.files.reset();
        self.source.clear();
        self.tests = Default::default();
        self.symbols.clear();
        self.library.clear();
        self.exploration = Default::default();
        self.trash_items = None;
        self.trash_error = None;
        self.reviews = Default::default();
        self.clear_extensions(window, cx);
        self.disconnect_chat_uploads();
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        self.comment_input.update(cx, |input, cx| input.set_value("", window, cx));
        self.comment_popup = None;
        self.comment_selection = None;
        self.palette = None;
        self.palette_graph = None;
        self.palette_search = None;
        self.chat_activity_clock = None;
        self.chat_waiting = Default::default();
        self.chats.clear();
        self.workspace_requests = Default::default();
        self.recent_transcripts.clear();
        self.github = Default::default();
        self.clear_github_report(window, cx);
        self.active.clear();
        self.chat("", cx);
        self.stats.clear();
        self.delegate_settings = None;
        self.worker_approval_policy = None;
        self.agent_settings = Default::default();
        self.agent_management = Default::default();
        self.agent_models.disconnect();
        self.chat_capabilities = None;
        self.codex_models.clear();
        self.review_settings = None;
        self.partner_default = None;
        self.thread_info = Default::default();
        self.history.reset();
        self.branch_review.reset();
        self.handoffs = Default::default();
        self.paper_jobs.clear();
        self.review_jobs.clear();
        self.review_clock = None;
        self.paper_job_result = None;
        self.pdf_audit.clear();
        self.open_proofs.clear();
        self.proof_comments.clear();
        self.built_pdfs.clear();
        self.paper_pdf = Default::default();
        self.paper_jump = None;
        self.pending_paper_open = None;
        self.paper_highlight = None;
        self.card_editor = None;
        self.card_tex_build = None;
        self.active_tab = None;
        self.deck.clear();
        self.tabs.clear();
        self.workspace_title = None;
        self.threads.clear();
        self.threads_loaded = false;
        self.thread_title_edit = None;
        self.pending_research_origin = None;
        self.cards.clear();
        self.card_diags.clear();
        self.lazy_deck = Default::default();
        self.document_deck = None;
        self.pdfs.clear();
        self.issues.clear();
        self.repo.clear();
        self.issue_error = None;
        self.deck_overview = Default::default();
        self.workshop.repo.clear();
        self.workshop.issues.clear();
        self.workshop.error = None;
        self.workshop.loaded = false;
        self.workshop.approvals.clear();
        self.usage.clear();
        self.selected = None;
        self.comments.clear();
        self.comment_target = None;
        self.open_comments.clear();
        self.lean.clear();
        self.lean_running = false;
        self.lean_doc_findings.clear();
        self.close_right_pane(cx);
        if let Some((_, pdf)) = self.pdf.take() {
            pdf.update(cx, |pdf, cx| pdf.release(window, cx));
        }
        self.pdf_status = None;
        self.pdf_path = None;
        self.pdf_digest = None;
        self.note = None;
        self.editing = None;
        self.from_pdf = false;
        self.pdf_goto = None;
        self.notice = None;
        self.translation_save_request = None;
        self.findings = Default::default();
        self.doc_title.clear();
        self.doc_sections.clear();
        self.doc_macros.clear();
        self.doc_math_macros.clear();
        self.doc_references = None;
        self.rebuild_read(false, cx);
    }

    fn on_event(&mut self, ev: ServerEvent, cx: &mut Context<Self>) {
        self.apply_event(ev, self.receiving_backlog, cx);
    }

    fn apply_event(&mut self, ev: ServerEvent, replaying: bool, cx: &mut Context<Self>) {
        self.apply_event_with_transcript(ev, replaying, true, cx);
    }

    fn apply_event_with_transcript(&mut self, ev: ServerEvent, replaying: bool, render: bool, cx: &mut Context<Self>) {
        if replaying && ev.is_live_only() {
            return;
        }
        if let ServerEvent::UserPrompt { session, .. } = &ev {
            if !replaying {
                self.thread_info.record_user_prompt(session);
            }
        }
        match ev {
            ServerEvent::Notify { .. } => {}
            ServerEvent::GitHubReportResult { request, result } => self.github_report_result(request, result, cx),
            ServerEvent::GitHubAccountChanged { state } => self.github_account_changed(state, cx),
            ServerEvent::GitHubAvatar { avatar } => self.github_avatar(avatar, cx),
            ServerEvent::DeckSearchResults { query, decks } => {
                if self.palette.as_ref().is_some_and(|input| input.read(cx).value().trim().to_lowercase() == query) {
                    self.palette_graph = Some((query, decks));
                    cx.notify();
                }
            }
            ServerEvent::PartnerDefaultChanged { settings } => {
                self.agent_models.partner_saved(&settings);
                self.partner_default = Some(settings);
            }
            ServerEvent::AgentModelCapabilities { claude_available, codex_available, front_desk_configured } => {
                self.agent_models.connected(claude_available, codex_available, front_desk_configured);
            }
            ServerEvent::DelegateSettingsChanged { settings } => self.delegate_settings = Some(settings),
            ServerEvent::WorkerApprovalPolicyChanged { policy, .. } => self.worker_approval_policy = Some(policy),
            ServerEvent::ReviewSettingsChanged { settings } => self.review_settings = Some(settings),
            ServerEvent::ClientCommand { command } => self.on_client_command(command, cx),
            event @ (ServerEvent::ChatCapabilities { .. } | ServerEvent::ChatSettingsChanged { .. }
            | ServerEvent::CodexModels { .. } | ServerEvent::CodexPartner { .. }
                 | ServerEvent::FolderReferenceUnavailable { .. }
                 | ServerEvent::AttachmentProgress { .. } | ServerEvent::AttachmentUploaded { .. } | ServerEvent::InboxFiled { .. }
                | ServerEvent::GitHubConnectCard { .. } | ServerEvent::TurnActivity { .. }
                | ServerEvent::ChatActivity { .. } | ServerEvent::ChatWaiting { .. } | ServerEvent::BackgroundCommands { .. }
                | ServerEvent::Consulting { .. }) => self.on_chat_event(event, cx),
            event @ (ServerEvent::GitHubActivity { .. } | ServerEvent::LocalNotice { .. }
                | ServerEvent::UserPrompt { .. }) if render => self.on_chat_event(event, cx),
            ServerEvent::TrashItems { items } => {
                self.trash_error = None;
                self.trash_items = Some(items);
                if self.right == RightPane::Library && !self.ledger_open() {
                    self.reset_library_search(false, cx);
                }
            }
            ServerEvent::TrashFailed { message } => {
                let message = tr!("trash-error", detail = message);
                if self.pane_close_id().is_some() {
                    self.trash_error = Some(message);
                } else {
                    let session = self.active.clone();
                    self.chat(&session, cx).push_error(message, cx);
                }
            }
            ServerEvent::TrashChanged => {
                self.refresh_trash();
                if self.right == RightPane::Library && !self.ledger_open() {
                    self.reset_library_search(false, cx);
                }
            }
            ServerEvent::ThreadRestored { session, events } => {
                self.chats.remove(&session);
                for event in events {
                    self.apply_event(event, true, cx);
                }
            }
            ServerEvent::ReplySuperseded { session, items } if render => {
                self.chat(&session, cx).supersede(&items, cx);
            }
            ServerEvent::Threads { threads } => {
                self.update_chat_macros(&threads, cx);
                for thread in threads.iter().filter(|thread| thread.github.unread > 0) {
                    if let Some(chat) = self.chats.get(&thread.id) {
                        let unread = self.github.unread.entry(thread.id.clone()).or_default();
                        for id in chat.recent_github_ids(thread.github.unread) {
                            if !unread.iter().any(|entry| entry == id) {
                                unread.push(id.to_owned());
                            }
                        }
                    }
                }
                if self.protocol >= 13 { self.sync_auto_attachments(&threads, cx); }
                let previous = self.threads.iter().find(|thread| thread.id == self.active);
                let incoming = threads.iter().find(|thread| thread.id == self.active);
                let archived_now = incoming.is_some_and(|thread| thread.archived)
                    && if self.transcripts.scoped {
                        previous.is_some_and(|thread| !thread.archived)
                    } else {
                        !previous.is_some_and(|thread| thread.archived)
                    };
                let removed = previous.is_some() && incoming.is_none();
                let pick_active = archived_now || removed || (!self.threads_loaded && incoming.is_none());
                if removed {
                    self.chats.remove(&self.active);
                    self.recent_transcripts.retain(|id| id != &self.active);
                }
                let artifacts_changed = incoming.map(|t| &t.artifacts) != previous.map(|t| &t.artifacts);
                if self.transcripts.scoped {
                    self.prune_thread_history(&threads);
                }
                self.threads_loaded = true;
                self.threads = threads;
                if pick_active {
                    let next = self
                        .threads
                        .iter()
                        .filter(|thread| !thread.archived)
                        .max_by_key(|thread| (thread.updated, &thread.id))
                        .map(|thread| thread.id.clone())
                        .unwrap_or_default();
                    if self.active != next {
                        self.select_thread(next, cx);
                    }
                } else if artifacts_changed {
                    self.request_thread_artifacts();
                }
            }
            ServerEvent::CommentApplication { session, application } if render => {
                self.chat(&session, cx).comment_application(application, cx);
            }
            ServerEvent::HistoryChanged { changed_turns } => self.history_changed(changed_turns, cx),
            ServerEvent::HistoryResult { id, outcome } => self.history_result(id, outcome),
            ServerEvent::Handoffs { notes } => self.handoffs.notes = notes,
            ServerEvent::HandoffOpened { session } => self.handoff_opened(session, cx),
            ServerEvent::HandoffGenerated { path } => self.handoff_generated(path),
            ServerEvent::HandoffFailed { message } => self.handoff_failed(message),
            ServerEvent::HandoffDetail { detail } => self.handoff_detail(detail),
            ServerEvent::HandoffWorkerUpdated { session, thread_id, status } => self.handoff_worker_updated(session, thread_id, status),
            ServerEvent::BranchReviewResult { id, outcome } => self.branch_review_result(id, outcome),
            ServerEvent::GitChanged => self.refresh_workspace_git(),
            ServerEvent::GitResult { id, outcome } => self.workspace_git_result(id, outcome),
            ServerEvent::ThreadArtifacts { session, artifacts } => {
                self.thread_info.artifacts.insert(session, artifacts);
            }
            ServerEvent::CompactStatus { session, state, message } => {
                self.thread_info.compaction.insert(session, (state, message));
            }
            ServerEvent::OpenPaper { session, deck, card } if session == self.active => {
                self.open_paper_from_chat(deck, card, cx);
            }
            ServerEvent::CardTexBuildResult { request, outcome } => self.on_card_tex_build_result(request, outcome),
            ServerEvent::CardSaveResult { request, outcome } => {
                if self.translation_save_request == Some(request) {
                    self.on_translation_save_result(outcome);
                } else {
                    self.on_card_save_result(request, outcome, cx);
                }
            }
            ServerEvent::ThreadCreated { id } => {
                if let Some(text) = self.auto_prompt.take() {
                    let _ = self.out.send(ClientMsg::Prompt { session: id.clone(), text });
                }
                self.select_thread(id, cx);
            }
            ServerEvent::AgentSettingsChanged { settings } => self.agent_settings = settings,
            ServerEvent::Agents { agents } => self.thread_info.agents = agents,
            ServerEvent::AgentStats { stats, .. } => self.agent_management.stats = Some(stats),
            ServerEvent::AgentHistory { agent, session, entries } => {
                let state = &mut self.agent_management;
                if state.agent == agent && state.session == session {
                    state.history = Some(entries);
                }
            }
            ServerEvent::AgentUsageChanged => self.refresh_agent_usage(),
            ServerEvent::PlanChanged { session, plan, progress, parallel_limits } => {
                self.thread_info.parallel_limits = parallel_limits;
                self.thread_info.update_plan(session, plan, progress);
            }
            ServerEvent::DelegateRenamed { session, job, change } if render => {
                self.chat(&session, cx).delegate_renamed(&job, &change, cx);
            }
            ServerEvent::DelegateDismissed { session, job } if render => {
                self.chat(&session, cx).delegate_dismissed(&job);
            }
            ServerEvent::ThreadJobActivity { session, job, tools } if render => {
                self.chat(&session, cx).update_job_tool_count(&job, tools, cx);
            }
            ServerEvent::DelegateProgress { session, job, summary } if render => {
                self.chat(&session, cx).delegate_progress(&job, summary, cx);
            }
            ServerEvent::TextDelta { session, item, actor, text } if render => {
                self.chat(&session, cx).push_text(item, actor, text, cx);
            }
            ServerEvent::ToolCall { session, item, actor, name, input } => {
                // 今の会話のエージェントがカードのファイルを書き換えたら、そのカードを右に開く。
                if !replaying && self.right != RightPane::Read && session == self.active
                    && matches!(name.as_str(), "Edit" | "Write" | "MultiEdit" | "edit")
                {
                    let hit = self.cards.iter().find(|c| {
                        fuzukue_view::chat::tool_paths(&input)
                            .any(|p| p.ends_with(&c.path) || c.tex.iter().any(|t| p.ends_with(t.as_str())))
                    });
                    if let Some(id) = hit.map(|c| c.id.clone()) {
                        self.select(id, cx);
                    }
                }
                if render {
                    let chat = self.chat(&session, cx);
                    if replaying {
                        chat.push_replayed_tool(item, actor, &name, input, cx);
                    } else {
                        chat.push_tool(item, actor, &name, input, cx);
                    }
                }
            }
            ServerEvent::LeanReadingCards { request, cards } => self.receive_lean_cards(request, cards),
            ServerEvent::LeanStatus { decls, running, doc_findings } => {
                self.lean_running = running;
                if !decls.is_empty() || !running {
                    self.lean = decls.into_iter().collect();
                    self.lean_doc_findings = doc_findings;
                }
            }
            ServerEvent::Comments { deck, items } => {
                if deck == self.deck {
                    self.comments = items;
                    self.open_commented_proofs(cx);
                    for (part, text) in &self.read_md {
                        self.highlight_quotes(part, &text.view, cx);
                    }
                    self.read_list.remeasure_items(0..self.read_rows.len());
                }
            }
            ServerEvent::CommentsSent { deck, session } => {
                self.chat(&session, cx).scroll_to_end();
                if self.deck == deck && self.active == session && self.right == RightPane::Read {
                    self.close_right_pane(cx);
                }
            }
            ev @ ServerEvent::SessionStats { .. } => {
                if let ServerEvent::SessionStats { session, agent, .. } = &ev {
                    self.stats.insert((session.clone(), agent.clone()), ev.clone());
                }
            }
            ServerEvent::TestSuites { suites, error } => self.update_test_suites(suites, error),
            ServerEvent::TestRun { run } => self.update_test_run(run),
            ServerEvent::ReviewReports { deck, revision, submissions, splits, fixing } => self.on_review_reports(deck, revision, submissions, splits, fixing, cx),
            ServerEvent::ReviewReportsResult { deck, request, ok } => self.on_review_result(&deck, request, ok),
            ServerEvent::ReviewFindings { deck, outcome } => self.on_review_findings(&deck, outcome),
            ServerEvent::SelfUpdateStatus { status } => self.update_self_update(status, cx),
            ServerEvent::SelfUpdateError { message } => {
                if let Some(status) = &mut self.self_update.status {
                    status.error = Some(message);
                }
            }
            ServerEvent::ClientUpdateAvailable => self.check_client_update(cx),
            ServerEvent::Welcome { protocol, client_manifest, server_tailnet_url, selection_scoped_transcripts, .. } => {
                self.transcripts.scoped = selection_scoped_transcripts;
                updater::connected();
                self.status = ConnectionStatus::Connected;
                if protocol < fuzukue_proto::SELF_UPDATE_PROTOCOL
                    || self.self_update.status.as_ref().is_some_and(|status| status.canary)
                {
                    self.self_update = Default::default();
                }
                if protocol >= fuzukue_proto::SELF_UPDATE_PROTOCOL && self.settings_open && self.sidebar.server_settings {
                    let _ = self.out.send(ClientMsg::SelfUpdateStatus);
                }
                if protocol >= fuzukue_proto::DISTRIBUTION_PROTOCOL {
                    self.distribution.public_url = server_tailnet_url;
                    if client_manifest.is_some() {
                        self.accept_client_manifest(client_manifest, cx);
                    }
                }
                self.invalidate_paper_pdf_maps();
                self.protocol = protocol;
                self.source_lsp_connected(cx);
                if self.right == RightPane::Library {
                    if self.ledger_open() {
                        self.open_ledger(cx);
                    } else {
                        self.reset_library_search(false, cx);
                    }
                }
                if protocol >= fuzukue_proto::LOCALE_PROTOCOL {
                    let locale = i18n::current().tag().into();
                    let _ = self.out.send(ClientMsg::SetLocale { locale });
                }
                if protocol >= fuzukue_proto::TESTS_PROTOCOL && self.right == RightPane::Tests {
                    self.refresh_test_suites(cx);
                }
                if protocol < 11 {
                    // Older servers choose the workspace without a Hello ID.
                    self.http.select_workspace(None);
                    self.load_workspace_title(cx);
                    if protocol >= 4 && self.auto_prompt.is_some() {
                        let _ = self.out.send(ClientMsg::NewThread);
                    }
                }
            }
            ServerEvent::PaperJobsReset => {
                self.paper_jobs.clear();
                self.paper_job_result = None;
                self.pdf_audit.clear();
                self.review_jobs.retain(|_, job| job.started_at.is_none());
                self.review_clock = None;
            }
            ServerEvent::ReviewDecksPruned { decks } => {
                self.review_jobs.retain(|(deck, _), _| !decks.contains(deck));
            }
            ServerEvent::MessageDetails { session, item, details } if render => {
                self.chat(&session, cx).set_message_details(&item, details, cx);
            }
            ServerEvent::ReviewStatus { deck, kind, state, message, model, started_at, count, card, details } => {
                let failed = state == "failed";
                self.update_review_job(deck, kind, state, paper::ReviewProgress { message, model, failed, started_at, count, card, details, _dismiss_timer: None }, cx);
            }
            ServerEvent::BuildStatus { deck, state, message, pdf } => {
                self.update_paper_job(deck.clone(), None, &state, message, cx);
                if let Some(pdf) = pdf {
                    self.built_pdfs.insert(deck.clone(), pdf.clone());
                    self.refresh_other_paper_pdf(&deck, &pdf, cx);
                }
            }
            ServerEvent::PaperPdfMap { deck, pdf, digest, regions, rebuilt } => {
                self.receive_paper_pdf_map(deck, pdf, digest, regions, rebuilt, cx);
            }
            ServerEvent::BuildResult { ok, pdf, log, pdf_audit, .. } => {
                self.pdf_audit = pdf_audit;
                if ok {
                    self.notice = Some(tr!("workbench-built", pdf = pdf.to_string()).into());
                    if self.right != RightPane::Read && self.pdf.as_ref().is_none_or(|(p, _)| *p != pdf) {
                        self.open_artifact_pdf(pdf, None, cx);
                    }
                } else if self.protocol < 3 {
                    self.notice = Some(tr!("workbench-build-pdf-failed", detail = truncate(&log, 400).to_string()).into());
                }
            }
            ServerEvent::TurnEnded { session, actor } => {
                if render {
                    self.chat(&session, cx).end_turn(&actor, cx);
                } else if let Some(chat) = self.chats.get_mut(&session) {
                    chat.finish_turn_activity(&actor, cx);
                }
                if session == self.active {
                    self.request_thread_artifacts();
                }
            }
            ServerEvent::GitHubItems { repo, items, error, retry_at } => {
                self.github.repo = repo;
                self.github.items = items;
                self.github.error = error;
                self.github.retry_at = retry_at;
            }
            ServerEvent::GitHubReply { session, event, error } => self.github_reply_result(&session, event, error, cx),
            ServerEvent::IssueIntakeRepos { repos } => self.github.intake_repos = repos,
            ServerEvent::ResearchIssues { repo, issues, error } => {
                self.workshop.loaded = true;
                self.workshop.repo = repo;
                self.workshop.issues = issues;
                self.workshop.error = error;
            }
            ServerEvent::Issues { repo, issues, error } => {
                self.deck_overview.issues_received(error.is_none());
                self.repo = repo;
                self.issues = issues;
                self.issue_error = error;
                // 動作確認用: 起動時に開いておく Issue。
                if let Some(n) = fuzukue_proto::compat::env("FUZUKUE_ISSUE").ok().and_then(|v| v.parse().ok()) {
                    if self.right == RightPane::None && self.issues.iter().any(|i| i.number == n) {
                        self.right = RightPane::Issue(n);
                    }
                }
            }
            ServerEvent::Usage { windows, pause_at } => {
                self.usage = windows;
                self.pause_at = pause_at;
            }
            ServerEvent::Paused { session, until, reason, queued } => self.chat(&session, cx).paused = Some((until, reason, queued)),
            ServerEvent::Resumed { session } => self.chat(&session, cx).paused = None,
            ServerEvent::ToolResult { session, item, ok, output } if render => {
                self.chat(&session, cx).tool_result(&item, ok, output, cx);
            }
            ServerEvent::Cards { deck, cards, diagnostics } => {
                self.lazy_deck = Default::default();
                self.apply_deck_cards(&deck, cards, diagnostics, cx);
            }
            ServerEvent::DeckCardsChanged { deck } => self.load_deck_summaries(deck, cx),
            ServerEvent::Workspace { tabs, deck } => {
                self.tabs = tabs;
                if self.deck != deck {
                    self.close_findings();
                    self.invalidate_review_findings();
                    self.dismiss_pdf_popup(cx);
                    self.comments.clear();
                    self.proof_comments.clear();
                    self.open_comments.clear();
                    self.comment_target = None;
                    self.comment_popup = None;
                    self.comment_selection = None;
                    for (part, text) in &self.read_md {
                        self.highlight_quotes(part, &text.view, cx);
                    }
                    self.paper_jump = None;
                    self.paper_highlight = None;
                    self.doc_references = None;
                    self.document_deck = None;
                    self.from_pdf = false;
                    self.read_list.remeasure_items(0..self.read_list.item_count());
                    self.read_list.scroll_to(gpui_kit::ListOffset::default());
                }
                self.deck = deck;
                self.load_review_reports(&self.deck);
                // 動作確認用: 起動時に論文編集の画面を開くデッキ。
                if let Ok(paper) = fuzukue_proto::compat::env("FUZUKUE_PAPER") {
                    if self.right == RightPane::None && self.active_tab.is_none() {
                        self.open_paper_deck(paper, cx);
                    }
                }
            }
            ServerEvent::Document { title, sections, macros, mut math_macros, references } => {
                self.document_deck = Some(self.deck.clone());
                self.doc_title = title;
                // Keep the last PDF's numbering while a body-only change is rebuilt.
                let mut presentation_changed = false;
                if references.is_some() || self.doc_sections != sections {
                    presentation_changed = self.doc_references != references;
                    self.doc_references = references;
                }
                self.doc_sections = sections;
                if math_macros.is_empty() {
                    math_macros = macros.iter().map(|(name, body)| fuzukue_proto::MathMacro {
                        name: name.clone(), body: body.clone(), arguments: 0,
                    }).collect();
                }
                presentation_changed |= self.doc_macros != macros || self.doc_math_macros != math_macros;
                self.doc_macros = macros;
                self.doc_math_macros = math_macros;
                self.rebuild_read(presentation_changed, cx);
                self.finish_paper_open(cx);
                // 動作確認用: 起動時に通して読む画面を開く。
                if fuzukue_proto::compat::env("FUZUKUE_READ").is_ok() && self.right == RightPane::None && !self.doc_sections.is_empty() {
                    self.right = RightPane::Read;
                }
            }
            ServerEvent::Artifacts { pdfs } => {
                self.pdfs = pdfs;
                // 動作確認用: 起動時に開いておく PDF。
                if self.pdf.is_none() && self.pdf_status.is_none() {
                    if let Ok(want) = fuzukue_proto::compat::env("FUZUKUE_PDF") {
                        if self.pdfs.contains(&want) {
                            self.open_artifact_pdf(want, None, cx);
                        }
                    }
                }
            }
            ServerEvent::Located { card, file, line, pdf } if self.right == RightPane::Pdf
                && self.pdf_path.as_ref() == Some(&pdf) => match card {
                Some(id) if self.cards.iter().any(|c| c.id == id) => {
                    self.jump_to_card(id, cx);
                    self.from_pdf = true;
                    self.notice = None;
                }
                Some(id) => self.notice = Some(tr!("workbench-source-card-missing", file = file.to_string(), line = line.to_string(), id = id.to_string()).into()),
                None => self.notice = Some(tr!("workbench-is-outside-any-card", file = file.to_string(), line = line.to_string()).into()),
            },
            ServerEvent::Saved { path } => {
                if self.protocol < fuzukue_proto::EDITOR_PROTOCOL
                    && self.editing.as_ref().is_some_and(|editor| editor.path == path)
                {
                    self.editing = None;
                }
                self.notice = Some(tr!("workbench-saved", path = path.to_string()).into());
            }
            ServerEvent::FileChanged { path } => {
                if self.pdf_path.as_ref() == Some(&path) {
                    if self.protocol >= 10 && self.paper_pdf_visible() {
                        self.request_current_pdf_map(&path, cx);
                    } else if self.pdf.is_some() {
                        self.reload_pdf(path, cx);
                    }
                }
            }
            ServerEvent::FolderGrants { session, grants, audit, audit_before } => {
                self.thread_info.folders.insert(session, thread_info::FolderDetails { grants, audit, audit_before });
            }
            ServerEvent::FolderGrantsChanged { session } => {
                if self.active == session {
                    self.request_folder_grants(None);
                }
            }
            ServerEvent::ApprovalRequested { session: _, request_id, actor: fuzukue_proto::Actor::System, summary, localized, .. }
                if request_id.starts_with(fuzukue_proto::RESEARCH_ISSUE_APPROVAL) => {
                if !self.workshop.approvals.iter().any(|(id, _)| id == &request_id) {
                    self.workshop.approvals.push((request_id, i18n::Message { fallback: summary, localized }));
                }
            }
            ServerEvent::ApprovalRequested { session, request_id, actor, summary, localized, .. } => {
                let summary = i18n::Message { fallback: summary, localized };
                if actor == fuzukue_proto::Actor::System && request_id.starts_with(fuzukue_proto::GIT_APPROVAL) {
                    self.thread_info.git.show_approval(&session, &request_id, &summary);
                }
                self.chat(&session, cx).request_approval(request_id, actor, summary, cx);
            }
            ServerEvent::ApprovalResolved { request_id, decision, automatic } => {
                self.thread_info.git.clear_approval(&request_id);
                self.workshop.approvals.retain(|(id, _)| id != &request_id);
                for chat in self.chats.values_mut() {
                    chat.resolve_approval(&request_id, &decision, automatic, cx);
                }
            }
            ServerEvent::QuestionAsked { session, request_id, actor, questions } => {
                let lines = 3 + questions.iter().map(|question| {
                    2 + question.options.len() + usize::from(question.allow_other)
                }).sum::<usize>();
                let card = self.question_drafts.entry(request_id.clone()).or_insert_with(|| {
                    let out = self.out.clone();
                    cx.new(|cx| chat_question::QuestionCard::new(request_id.clone(), questions, out, cx))
                }).clone();
                self.chat(&session, cx).add_question(request_id, actor, card, lines, cx);
            }
            ServerEvent::QuestionResolved { session, request_id, outcome, answers } => {
                if let Some(card) = self.question_drafts.remove(&request_id) {
                    card.update(cx, |card, cx| card.resolve(outcome, answers, cx));
                    if let Some(chat) = self.chats.get_mut(&session) {
                        chat.question_resolved(&request_id, cx);
                    }
                }
            }
            ServerEvent::WorkspaceRequests { view } => {
                self.workspace_requests.available = true;
                self.workspace_requests.view = view;
            }
            ServerEvent::WorkspaceRequestDrafts { drafts, other_workspaces, complete } => {
                self.workspace_requests.receive_drafts(drafts, other_workspaces, complete);
            }
            ServerEvent::WorkspaceRequestError { message } => {
                let text = i18n::Message { fallback: message.key.clone(), localized: Some(message) };
                self.workspace_requests.error = Some(text.text().into_owned());
            }
            ServerEvent::ResponseTooLarge { message } => self.notice = Some(message.into()),
            ServerEvent::Error { message } => {
                self.agent_models.save_failed();
                let session = self.active.clone();
                self.chat(&session, cx).push_error(message, cx);
            }
            _ => {}
        }
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Kit の open_window は Base Root を使うので、rem はここでテーマに合わせる。
        window.set_rem_size(cx.theme().font_size);
        self.sync_chat_activity_clock(cx);
        self.sync_blocker_expiry(cx);
        self.ensure_paper_pdf(window, cx);
        if self.right == RightPane::Card {
            if let Some(card) = self.selected.clone() {
                self.load_card_content(card, cx);
            }
        }
        let theme = cx.theme().clone();
        let source_centered = self.right == RightPane::Source && self.source.centered;
        let paper_info = self.thread_info_visible()
            && (self.right == RightPane::Read || source_centered) && !self.settings_open;
        if !self.settings_open {
            self.offer_model_setup();
        }
        let chat_col = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .children(self.render_thread_header(cx))
            .children(self.handoff_thread_button(cx))
            .children((!self.settings_open).then(|| self.render_model_setup(false, cx)).flatten())
            .children((!self.settings_open && !self.thread_info_visible()).then(|| self.render_update_notice(cx)).flatten());
        let symbols = self.render_symbol_panel(cx).filter(|_| !self.settings_open);
        let chat_visible = symbols.is_none() && !paper_info;
        let chat_col = if let Some(panel) = symbols {
            let panel = if self.pane_close_id() == Some("symbol-close") {
                self.render_pane_notice(panel.into_any_element(), cx)
            } else {
                panel.into_any_element()
            };
            v_flex().flex_1().h_full().min_w_0().child(panel)
        } else if paper_info {
            chat_col.child(self.render_thread_info(cx))
        } else {
            // A full attach can paint between replay batches. Wait for the thread
            // snapshot before showing rows from the newly built transcript.
            let loading = self.receiving_backlog && (!self.threads_loaded || self.transcripts.scoped)
                && (self.workspaces.active.is_some() || self.protocol >= 4);
            chat_col
                .children(self.render_paper_jobs(window, cx))
                .child(div().flex_1().min_h_0()
                    .when(loading, |pane| pane.flex().items_center().justify_center()
                        .text_sm().text_color(theme.muted_foreground).child(tr!("workbench-loading")))
                    .when(!loading, |pane| pane.child(self.render_chat(window, cx))))
                .children(self.render_workspace_request_chat(cx))
                .child(self.render_chat_footer(window, cx))
                .children(self.render_approval_dock(window, cx))
                .child(
                    chat::chat_column().pt_2().pb_5()
                        .child(self.render_composer(window, cx))
                )
        };
        let uncovered = !self.workspace_browser_open() && self.palette.is_none()
            && !window.has_active_dialog(cx);
        self.report_workspace_request_surface(chat_visible, uncovered);
        let root = self.render_workbench_layout(chat_col, window, cx);
        div()
            .id("workbench")
            .track_focus(&self.view_focus)
            .relative()
            .size_full()
            .font(self.appearance.ui_font.clone())
            .text_size(theme.font_size)
            .line_height(appearance::UI_LINE_HEIGHT)
            .text_color(theme.foreground)
            .capture_any_mouse_up(cx.listener(|this, event: &gpui_kit::MouseUpEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.finish_comment_selection(event.position, window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                // IME の確定は OS に任せ、変換中の Enter をアプリの操作に渡さない。
                if focused_composition(&this.input, window, cx)
                    || focused_composition(&this.comment_input, window, cx)
                    || this.palette.as_ref().is_some_and(|input| focused_composition(input, window, cx))
                    || this.editing.as_ref().is_some_and(|editor| focused_composition(&editor.input, window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .capture_key_down(cx.listener(|this, ev: &gpui_kit::KeyDownEvent, window, cx| {
                let mods = ev.keystroke.modifiers;
                if mods.secondary(cx) && !mods.alt {
                    let delta = match ev.keystroke.key.as_str() {
                        "=" | "+" => 1.0,
                        "-" => -1.0,
                        _ => return,
                    };
                    this.change_text_size(delta, window, cx);
                    cx.stop_propagation();
                }
            }))
            // Ctrl+K（Mac は ⌘K）で探す、Esc で閉じる
            .on_key_down(cx.listener(|this, ev: &gpui_kit::KeyDownEvent, window, cx| {
                let k = ev.keystroke.key.as_str();
                if k == "escape" && !window.has_active_dialog(cx) && this.close_on_escape(window, cx) {
                    cx.stop_propagation();
                } else if !this.workspace_browser_open() && k == "k" && ev.keystroke.modifiers.secondary(cx) {
                    this.open_palette(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex().size_full()
                    .child(self.render_menu_bar(window, cx))
                    .children(self.render_workspace_request_notice(chat_visible, cx))
                    .child(div().flex_1().min_h_0().w_full().child(root)),
            )
            .children(self.render_pdf_popup(window, cx))
            .children(self.render_history(cx))
            .children(self.render_palette(cx))
            .children(self.render_screenshots(window, cx))
            .children(self.render_workspace_browser(cx).map(|browser| gpui_kit::deferred(browser).with_priority(10)))
    }
}

impl Workbench {
    fn close_workbench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_can_close(window, cx) {
            window.remove_window();
        }
    }

    fn pane_close_id(&self) -> Option<&'static str> {
        Some(if self.settings_open {
            "close-settings"
        } else if self.right == RightPane::Files {
            "close-file-browser"
        } else if self.symbol_view_visible() {
            "symbol-close"
        } else if self.reviews_visible() && self.right == RightPane::Read {
            "close-review"
        } else if self.right == RightPane::Read && self.paper_pdf_visible() {
            "close-paper-pdf"
        } else if self.details_view_visible() && self.right != RightPane::Source {
            return None;
        } else {
            match self.right {
                RightPane::None => return None,
                RightPane::Library if self.ledger_open() => "ledger-close",
                RightPane::Library => "library-close",
                RightPane::Extension => "extension-close-view",
                RightPane::Source => "close-source",
                RightPane::Trash => "trash-close",
                RightPane::BranchReview => "branch-review-close",
                RightPane::Findings => "review-findings-close",
                RightPane::Handoff => "handoff-close",
                RightPane::ResearchIssues | RightPane::ResearchIssue(_) => "research-issues-close",
                RightPane::Exploration => "exploration-close",
                RightPane::Read => "close-paper",
                _ => "close-pane",
            }
        })
    }

    fn symbol_view_visible(&self) -> bool {
        self.symbols.is_open() && !matches!(self.right, RightPane::Library | RightPane::Trash | RightPane::Extension | RightPane::BranchReview | RightPane::Handoff | RightPane::ResearchIssues | RightPane::ResearchIssue(_) | RightPane::Exploration)
    }

    fn details_view_visible(&self) -> bool {
        self.thread_info_visible() && !matches!(self.right, RightPane::Library | RightPane::Trash | RightPane::Extension | RightPane::BranchReview | RightPane::Handoff | RightPane::ResearchIssues | RightPane::ResearchIssue(_) | RightPane::Exploration)
    }

    fn pane_close_button(&self, id: &'static str, cx: &Context<Self>) -> impl IntoElement {
        use gpui_kit::component::{button::{Button, ButtonVariants as _}, Sizable as _};
        Button::new(id).ghost().xsmall().flex_none().icon(Symbol::X.icon())
            .tooltip(tr!("pane-close")).accessibility_label(tr!("pane-close"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_top_view(window, cx);
            }))
    }

    fn pane_heading(&self, content: impl IntoElement, cx: &Context<Self>) -> gpui_kit::Div {
        h_flex().gap_2().min_w_0().flex_none()
            .child(div().flex_1().min_w_0().child(content))
            .children((self.pane_close_id() == Some("close-pane"))
                .then(|| self.pane_close_button("close-pane", cx)))
    }

    fn close_on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_screenshot() {
            cx.notify();
            true
        } else if self.workspace_browser_open() {
            self.close_workspace_browser(window, cx);
            true
        } else {
            self.close_sidebar_menu(cx) || self.close_top_view(window, cx)
        }
    }

    fn close_top_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.palette.is_some() {
            self.close_palette();
        } else if self.right == RightPane::Files && !self.settings_open {
            self.close_file_browser(window, cx);
        } else if self.history.close() {
        } else if self.paper_pdf_visible() && self.paper_pdf.popup.is_some() {
            self.dismiss_pdf_popup(cx);
        } else if self.settings_open {
            self.settings_open = false;
        } else if self.symbol_view_visible() {
            self.symbols.clear();
        } else if self.close_card_file_editor(cx) {
        } else if self.right == RightPane::Findings {
            self.close_findings();
        } else if self.right == RightPane::Read && self.reviews_visible() {
            self.close_reviews();
        } else if self.right == RightPane::Read && self.paper_pdf_visible() {
            self.set_paper_pdf(false, cx);
        } else if self.right == RightPane::Library && self.close_ledger(cx) {
        } else if self.right == RightPane::Exploration {
            self.exploration = Default::default();
            self.right = RightPane::Overview;
        } else if self.right == RightPane::Source {
            self.close_source_view(window, cx);
        } else if self.details_view_visible() && self.thread_info.git.close_top() {
        } else if self.details_view_visible() {
            self.close_thread_info();
        } else if self.right != RightPane::None {
            self.close_right_pane(cx);
        } else {
            return false;
        }
        self.trash_error = None;
        cx.notify();
        true
    }

    fn close_right_pane(&mut self, cx: &mut Context<Self>) {
        self.pending_paper_open = None;
        self.deck_overview.clear_deck_request();
        if self.right == RightPane::BranchReview {
            self.branch_review.close();
        }
        if self.right == RightPane::Exploration {
            self.exploration = Default::default();
        }
        self.right = RightPane::None;
        cx.notify();
    }

    fn open_research_origin(&mut self, origin: ResearchOrigin, cx: &mut Context<Self>) {
        if self.deck == origin.deck {
            self.select(origin.card, cx);
        } else if self.out.send(ClientMsg::SelectDeck { deck: origin.deck.clone() }).is_ok() {
            self.pending_research_origin = Some(origin);
        }
    }

    fn apply_deck_cards(&mut self, deck: &str, mut cards: Vec<CardView>, diagnostics: Vec<CardDiagnostic>, cx: &mut Context<Self>) {
        self.findings.cards_deck = Some(wire_deck(&deck.replace('\\', "/")).to_owned());
        cards.sort_by(|a, b| (a.external, kind_rank(&a.kind), &a.id).cmp(&(b.external, kind_rank(&b.kind), &b.id)));
        if self.selected.as_ref().is_some_and(|id| !cards.iter().any(|c| &c.id == id)) {
            self.selected = None;
        }
        let previous = std::mem::replace(&mut self.cards, cards);
        self.card_diags = diagnostics;
        self.sync_card_file_style_notes(cx);
        self.rebuild_read(false, cx);
        let mut previous: HashMap<_, _> = previous.iter().map(|card| (card.id.as_str(), card)).collect();
        let mut changed = HashSet::new();
        for card in &self.cards {
            if previous.remove(card.id.as_str()).is_none_or(|old| {
                old.path != card.path || old.kind != card.kind || old.title != card.title
                    || old.status != card.status || old.lean != card.lean || old.external != card.external
                    || old.research_origin != card.research_origin
                    || old.edit_body.as_ref().map(|source| &source.tex) != card.edit_body.as_ref().map(|source| &source.tex)
                    || old.edit_proof.as_ref().map(|source| &source.tex) != card.edit_proof.as_ref().map(|source| &source.tex)
            }) {
                changed.insert(card.id.as_str());
            }
        }
        changed.extend(previous.into_keys());
        for (index, row) in self.read_rows.iter().enumerate() {
            if matches!(row, ReadRow::Card(id) if changed.contains(id.as_str())) {
                self.read_list.remeasure_items(index..index + 1);
            }
        }
        self.finish_overview_deck_request(deck);
        if self.pending_research_origin.as_ref().is_some_and(|origin| origin.deck == deck) {
            let origin = self.pending_research_origin.take().unwrap();
            if self.cards.iter().any(|card| card.id == origin.card) {
                self.select(origin.card, cx);
            } else {
                self.notice = Some(tr!("workbench-research-card-not-found").into());
            }
        }
    }

    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        self.load_card_content(id.clone(), cx);
        self.settings_open = false;
        self.selected = Some(id);
        self.right = RightPane::Card;
        self.from_pdf = false;
        cx.notify();
    }

    fn open_artifact_pdf(&mut self, path: String, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let deck = path.rsplit_once("/build/").map(|(deck, _)| deck)
            .or_else(|| path.starts_with("build/").then_some(""));
        if let Some(deck) = deck.filter(|deck| self.paper_deck(deck)).map(str::to_owned) {
            self.open_paper_pdf_artifact(deck, path, cx);
        } else {
            self.open_pdf(path, window, cx);
        }
    }

    /// PDF を開く。別の PDF を開いていたら、そのビューを片づけてから作り直す。
    fn open_pdf(&mut self, path: String, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.right = RightPane::Pdf;
        self.load_pdf(path, window, cx);
    }

    fn load_pdf(&mut self, path: String, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.pdf_path.as_ref() == Some(&path) {
            if let Some((_, view)) = &self.pdf {
                view.update(cx, |pdf, cx| pdf.set_outline_enabled(true, cx));
            }
            cx.notify();
            return;
        }
        if let (Some((_, old)), Some(window)) = (self.pdf.take(), window) {
            old.update(cx, |v, cx| v.release(window, cx));
        }
        self.pdf = None;
        self.pdf_digest = None;
        self.pdf_path = Some(path.clone());
        self.pdf_request += 1;
        let request = self.pdf_request;
        let fit_width = self.paper_pdf_visible();
        self.pdf_status = Some(tr!("workbench-fetching", path = path.to_string()).into());
        cx.notify();
        let epoch = self.connection_epoch;
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let got = cx.background_executor().spawn(async move { http.fetch(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch || this.pdf_request != request {
                    return;
                }
                match got {
                    Ok(bytes) => {
                        let style = this.appearance.pdf_style.clone();
                        let name = std::path::PathBuf::from(&path);
                        let view = cx.new(|cx| PdfView::from_bytes(name, bytes, Rc::new(move || style.get()), Rc::new(|| 1.0), cx));
                        if fit_width {
                            view.update(cx, |v, cx| {
                                v.fit_width(cx);
                                v.set_outline_enabled(false, cx);
                            });
                        }
                        let pdf_path = path.clone();
                        cx.subscribe(&view, move |this, view, ev: &gpui_pdf::PdfEvent, cx| {
                            if this.pdf.as_ref().is_some_and(|(_, active)| active == &view) {
                                this.on_pdf_event(&pdf_path, &view, ev, cx);
                            }
                        })
                        .detach();
                        if let Some(page) = this.pdf_goto.take() {
                            view.update(cx, |v, cx| v.reveal_highlight(page, cx));
                        }
                        this.pdf = Some((path, view));
                        this.pdf_status = None;
                    }
                    Err(e) => this.pdf_status = Some(tr!("workbench-cannot-open", path = path.to_string(), e = e.to_string()).into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 開いている PDF が書き換わった。スクロール位置と拡大率を保ったまま中身だけ替える。
    fn reload_pdf(&mut self, path: String, cx: &mut Context<Self>) {
        self.pdf_request += 1;
        let request = self.pdf_request;
        let epoch = self.connection_epoch;
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let got = cx.background_executor().spawn(async move { http.fetch(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch || this.pdf_request != request {
                    return;
                }
                if let (Ok(bytes), Some((cur, view))) = (got, this.pdf.as_ref()) {
                    if *cur == path {
                        view.update(cx, |v, cx| v.replace_bytes(bytes, cx));
                    }
                }
            });
        })
        .detach();
    }

    fn render_pdf_pane(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let theme = cx.theme().clone();
        let pane = v_flex().size_full().min_w_0().overflow_hidden().bg(theme.muted);
        match (&self.pdf, &self.pdf_status) {
            (Some((_, view)), _) => pane
                .child(self.pane_heading(
                    div().text_xs().text_color(theme.muted_foreground)
                        .child(tr!("workbench-ctrl-click-click-on-mac-or-double-click-to")), cx,
                ).px(PANE_INSET).py_1())
                .children(self.notice.clone().map(|n| {
                    div().px(PANE_INSET).py_1().text_xs().bg(theme.secondary).text_color(theme.muted_foreground).child(n)
                }))
                .child(div().flex_1().min_h_0().child(view.clone())),
            (None, status) => pane.child(self.pane_heading(
                div().text_sm().text_color(theme.muted_foreground).child(status.clone()
                    .unwrap_or_else(|| tr!("workbench-choose-a-pdf-under-output-to-show-it-here").into())), cx,
            ).px(PANE_INSET).py_1()),
        }
    }

    fn render_card_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let errors = self.card_diags.iter().filter(|d| d.error).count();
        let warnings = self.card_diags.len() - errors;
        let mut list = v_flex().gap_0p5();
        let heading = |t: String| div().pt_3().pb_1().text_xs().text_color(theme.muted_foreground).child(t);
        let nav = |id: String, label: String, selected: bool, icon: Option<Symbol>| {
            h_flex()
                .gap_2()
                .id(SharedString::from(id))
                .px_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .when(selected, |d| d.bg(theme.accent).text_color(theme.accent_foreground))
                .hover(|d| d.bg(theme.secondary))
                .children(icon.map(|icon| icon.tinted(theme.muted_foreground)))
                .child(label)
        };

        // 検索や添付から開いたデッキの概要
        if let Some(tab) = self.active_tab.as_ref().and_then(|id| self.tabs.iter().find(|t| &t.id == id)) {
            if let Some(deck) = tab.deck.clone().filter(|_| tab.plan.is_none()) {
                let label = match tab.kind.as_str() {
                    "research" => tr!("workbench-research-cards-plans-questions-and-ideas"),
                    "book" => tr!("workbench-book-cards"),
                    _ => tr!("workbench-cards"),
                };
                let current = deck == self.deck;
                list = list.child(heading(tr!("workbench-deck-overview").into())).child(
                    nav(format!("tabdeck-{}", tab.id), label.into(), current, Some(Symbol::LayoutList))
                        .child(Symbol::ChevronRight.tinted(theme.muted_foreground))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_overview_cards(&deck, cx))),
                );
                if current {
                    list = list.children(self.deck_findings_control("overview-review-findings", cx));
                }
                if !tab.next.is_empty() {
                    list = list.child(div().px_2().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-next-card", title = truncate(&tab.next, 140).to_string())));
                }
                if let Some(status) = self.deck_overview.deck_status() {
                    list = list.child(div().id("overview-deck-status").px_2().text_xs()
                        .text_color(theme.muted_foreground).child(status));
                }
            }
            if let Some(plan) = &tab.plan {
                let p = plan.clone();
                list = list.child(heading(tr!("workbench-plan").into())).child(
                    nav(format!("plan-{}", tab.id), "plan.md".into(), self.note.as_ref().is_some_and(|(n, _)| n == plan) && self.right == RightPane::Note, None)
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.open_note(p.clone(), cx))),
                );
                if !tab.next.is_empty() {
                    list = list.child(div().px_2().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-next-card", title = truncate(&tab.next, 140).to_string())));
                }
            }
            if let Some(deck) = tab.deck.clone().filter(|_| tab.has_exploration) {
                let title = tab.title.clone();
                list = list.child(nav("open-exploration".into(), tr!("exploration-title").into(), false, Some(Symbol::GitBranch))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_exploration(deck.clone(), title.clone(), cx))));
            }
            if tab.papers.iter().any(|p| p.decks.first() != tab.deck.as_ref()) {
                list = list.child(heading(tr!("workbench-manuscript").into()));
                for paper in tab.papers.iter().filter(|p| p.decks.first() != tab.deck.as_ref()).take(12) {
                    let deck = paper.decks.first().cloned();
                    let current = deck.as_ref().is_some_and(|d| *d == self.deck);
                    let icon = deck.as_ref().map(|_| Symbol::LayoutList);
                    let mut item = nav(format!("paper-{}", paper.name), paper.name.clone(), current, icon);
                    if let Some(deck) = deck {
                        item = item.on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, _| {
                            let _ = this.out.send(ClientMsg::SelectDeck { deck: deck.clone() });
                        }));
                    }
                    list = list.child(item);
                    for pdf in &paper.pdfs {
                        let p = pdf.clone();
                        let name = pdf.rsplit('/').next().unwrap_or(pdf).to_string();
                        list = list.child(
                            div()
                                .id(SharedString::from(format!("tabpdf-{pdf}")))
                                .pl_6()
                                .text_xs()
                                .text_color(theme.link)
                                .cursor_pointer()
                                .child(icon_label(Symbol::FileText, name, theme.muted_foreground))
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| this.open_artifact_pdf(p.clone(), Some(window), cx))),
                        );
                    }
                }
                if tab.papers.len() > 12 {
                    list = list.child(div().px_2().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-more-cards", count = (tab.papers.len() - 12).to_string())));
                }
            }
            if !tab.related.is_empty() {
                list = list.child(heading(tr!("workbench-connections").into()));
                let mut related = tab.related.clone();
                related.sort_by(|a, b| b.shared.len().cmp(&a.shared.len()));
                for r in related {
                    let title = self.tabs.iter().find(|t| t.id == r.id).map(|t| t.title.clone()).unwrap_or(r.id.clone());
                    let id = r.id.clone();
                    list = list.child(
                        nav(format!("rel-{}", r.id), title, false, Some(Symbol::ArrowLeftRight))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.select_tab(id.clone(), cx))),
                    );
                    list = list.child(div().pl_6().text_xs().text_color(theme.muted_foreground).child(truncate(&r.shared.join(", "), 120)));
                }
            }
            if !tab.notes.is_empty() {
                list = list.child(heading(tr!("workbench-notes-count", count = tab.notes.len().to_string())));
                for note in tab.notes.iter().filter(|n| n.matches('/').count() <= 2).take(15) {
                    let p = note.clone();
                    let name = note.rsplit('/').next().unwrap_or(note).to_string();
                    let open = self.right == RightPane::Note && self.note.as_ref().is_some_and(|(n, _)| n == note);
                    list = list.child(
                        nav(format!("note-{note}"), name, open, None)
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.open_note(p.clone(), cx))),
                    );
                }
            }
        }

        // 論文を通して読む
        if !self.doc_sections.is_empty() {
            list = list.child(heading(tr!("workbench-read").into())).child(
                nav("read-all".into(), tr!("workbench-read-through-deck", title = truncate(&self.doc_title, 24).to_string()), self.right == RightPane::Read, Some(Symbol::BookOpen))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.right = RightPane::Read;
                        cx.notify();
                    })),
            );
        }

        // GitHub の作業場
        list = list.child(
            h_flex()
                .justify_between()
                .child(heading(if self.repo.is_empty() { tr!("workbench-issues").into() } else { tr!("workbench-repository-issues", repo = self.repo.as_str()) }))
                .child(self.overview_issue_refresh_button(cx)),
        );
        if let Some(status) = self.deck_overview.issue_status() {
            list = list.child(div().id("issue-refresh-status").px_2().text_xs()
                .text_color(theme.muted_foreground).child(status));
        }
        if let Some(e) = &self.issue_error {
            list = list.child(div().id("overview-issue-error").px_2().text_xs().text_color(theme.muted_foreground).child(truncate(e, 200)));
        }
        if self.issues.is_empty() && self.issue_error.is_none() {
            list = list.child(div().px_2().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-no-open-issues")));
        }
        for issue in &self.issues {
            let state = if issue.labels.iter().any(|l| l == "dot:review") {
                Some(Symbol::Diamond)
            } else if issue.labels.iter().any(|l| l == "dot:running") {
                Some(Symbol::CircleDot)
            } else if issue.labels.iter().any(|l| l == "dot:ready") {
                Some(Symbol::Circle)
            } else {
                None
            };
            let n = issue.number;
            list = list.child(
                nav(format!("issue-{n}"), format!("#{n} {}", issue.title), self.right == RightPane::Issue(n), state)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                        this.right = RightPane::Issue(n);
                        this.github_view();
                        cx.notify();
                    })),
            );
        }

        list = list.children(self.deck_card_rows(cx));
        // 研究のタブでは、PDF は原稿の下に出しているので「出力」はプロジェクトのタブだけ。
        let project_tab = self.active_tab.as_deref().is_none_or(|t| t == "project");
        if !self.pdfs.is_empty() && project_tab {
            list = list.child(div().pt_4().pb_1().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-output")));
            for path in &self.pdfs {
                let p = path.clone();
                let open = self.right == RightPane::Pdf && self.pdf.as_ref().is_some_and(|(cur, _)| cur == path);
                let name = path.rsplit('/').next().unwrap_or(path).to_string();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("pdf-{path}")))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(open, |d| d.bg(theme.accent).text_color(theme.accent_foreground))
                        .hover(|d| d.bg(theme.secondary))
                        .child(icon_label(Symbol::FileText, name, theme.muted_foreground))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| this.open_artifact_pdf(p.clone(), Some(window), cx))),
                );
            }
        }
        let count = self.cards.iter().filter(|card| !card.external).count();
        let cards = tr!("workbench-card-count", count = count);
        let diag_line = if self.card_diags.is_empty() {
            cards
        } else {
            tr!("workbench-card-count-with-diagnostics", cards = cards, errors = errors, warnings = warnings)
        };
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                v_flex()
                    .px(PANE_INSET).py_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(self.pane_heading(div().text_sm().child(tr!("workbench-overview")), cx))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(diag_line))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(self.status.text().lines().next().unwrap_or_default().to_owned()))
                    .children(self.card_diags.iter()
                        .filter(|d| d.style.as_ref().is_some_and(|style| style.code == "style/textlint-status"))
                        .map(|d| div().text_xs().text_color(theme.warning).child(card_diagnostic_text(d))))
                    .children(self.usage_lines(cx)),
            )
            .child(div().id("card-list").flex_1().min_h_0().px_2().pb_3().overflow_y_scrollbar().child(list)
                .children(self.deck_diagnostics_panel(None, cx)))
    }

    /// 「Claude 5時間 12%（あと2時間3分）」を agent ごとに 1 行。閾値に近い枠は色を変える。
    fn usage_lines(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let mut agents: Vec<&str> = self.usage.iter().map(|w| w.agent.as_str()).collect();
        agents.dedup();
        agents
            .into_iter()
            .map(|agent| {
                let parts: Vec<String> = self
                    .usage
                    .iter()
                    .filter(|w| w.agent == agent)
                    .map(|w| {
                        let left = w.resets_at.map(|r| format!("（{}）", until_text(r))).unwrap_or_default();
                        format!("{} {:.0}%{left}", w.name, w.used * 100.0)
                    })
                    .collect();
                let hot = self.usage.iter().any(|w| w.agent == agent && (w.rejected || w.used >= self.pause_at * 0.85));
                let label = if agent == "claude" { "Claude" } else if agent == "codex" { "Codex" } else { agent };
                div()
                    .text_xs()
                    .text_color(if hot { theme.warning } else { theme.muted_foreground })
                    .child(format!("{label} · {}", parts.join(" · ")))
                    .into_any_element()
            })
            .collect()
    }

    fn render_issue_detail(&self, number: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pane = v_flex().size_full().min_w_0().overflow_hidden();
        let pane = pane.children(self.github_watch_status(cx));
        let Some(issue) = self.issues.iter().find(|i| i.number == number) else {
            let mut detail = div().id("github-issue-detail").flex_1().min_h_0()
                .px(PANE_INSET).py_4().overflow_y_scrollbar();
            if let Some(item) = self.github.items.iter().find(|item| item.link.number == number) {
                detail = detail.child(self.github_item_detail(item, cx));
            } else {
                detail = detail.child(self.pane_heading(div().child(tr!("workbench-loading")), cx))
                    .child(self.github_linked_threads(number, cx));
            }
            return pane.child(detail);
        };
        let has = |l: &str| issue.labels.iter().any(|x| x == l);
        let session = fuzukue_proto::issue_thread_id(number);
        let local_running = self.threads.iter().any(|thread| thread.id == session);

        let mut actions = h_flex().gap_2().flex_wrap();
        if local_running {
            let s2 = session.clone();
            actions = actions.child(
                pill(format!("open-{number}"), tr!("workbench-open-conversation"), true, cx)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.select_thread(s2.clone(), cx))),
            );
        } else {
            actions = actions.child(pill(format!("work-{number}"), tr!("workbench-work-on-this-locally"), true, cx).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    let _ = this.out.send(ClientMsg::WorkIssue { number });
                }),
            ));
        }
        if !has("dot:ready") && !has("dot:running") {
            actions = actions.child(pill(format!("ready-{number}"), tr!("workbench-mark-dot-ready"), false, cx).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    let _ = this.out.send(ClientMsg::SetIssueLabels { number, add: vec!["dot:ready".into()], remove: vec![] });
                }),
            ));
        }
        if has("dot:ready") {
            actions = actions.child(pill(format!("unready-{number}"), tr!("workbench-remove-dot-ready"), false, cx).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    let _ = this.out.send(ClientMsg::SetIssueLabels { number, add: vec![], remove: vec!["dot:ready".into()] });
                }),
            ));
        }
        let url = issue.url.clone();
        actions = actions.child(
            pill(format!("gh-{number}"), tr!("workbench-open-on-github"), false, cx).on_mouse_down(MouseButton::Left, move |_, _, cx| cx.open_url(&url)),
        );

        let mut cards = v_flex().gap_0p5();
        for id in &issue.cards {
            let exists = self.cards.iter().any(|c| &c.id == id);
            let target = id.clone();
            cards = cards.child(
                div()
                    .id(SharedString::from(format!("issue-card-{id}")))
                    .text_xs()
                    .child(icon_label(Symbol::ChevronRight, id.clone(), theme.muted_foreground))
                    .when(exists, |d| {
                        d.text_color(theme.link)
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.select(target.clone(), cx)))
                    })
                    .when(!exists, |d| d.text_color(theme.danger)),
            );
        }
        let branch = match &issue.branch {
            Some(b) => format!("{} · {} · {}", b.name, b.last_commit_at, b.last_message),
            None => tr!("workbench-no-working-branches-yet").into(),
        };
        pane.child(
            div().id("issue-detail").flex_1().min_h_0().w_full().overflow_y_scrollbar().child(
                v_flex()
                    .w_full()
                    .px(PANE_INSET).py_4()
                    .gap_3()
                    .child(self.pane_heading(div().text_lg().child(format!("#{} {}", issue.number, issue.title)), cx))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(tr!("workbench-issue-summary", author = (if issue.labels.is_empty() { tr!("workbench-no-labels").to_string() } else { issue.labels.join(", ") }).to_string(), count = issue.comments.to_string(), date = issue.updated_at.to_string())))
                    .child(actions)
                    .child(self.github_linked_threads(number, cx))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(icon_label(Symbol::GitBranch, branch, theme.muted_foreground)))
                    .child(cards)
                    .child(div().w_full().child(
                        TextView::markdown(SharedString::from(format!("issue-body-{number}")), link_card_refs(&issue.body))
                            .selectable(true)
                            .on_link_click(self.markdown_link_handler())
                    )),
            ),
        )
    }

    /// 出典の PDF を開いて、そのページへ移動する（ページが光る）。
    fn open_source(&mut self, pdf: String, page: usize, window: &mut Window, cx: &mut Context<Self>) {
        let index = page.saturating_sub(1);
        if let Some((cur, view)) = &self.pdf {
            if *cur == pdf {
                view.update(cx, |v, cx| v.reveal_highlight(index, cx));
                self.right = RightPane::Pdf;
                cx.notify();
                return;
            }
        }
        self.pdf_goto = Some(index);
        self.open_pdf(pdf, Some(window), cx);
    }

    fn render_card_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pane = v_flex().size_full().min_w_0().overflow_hidden();
        let Some(card) = self.selected.as_ref().and_then(|id| self.cards.iter().find(|c| &c.id == id)) else {
            return pane.child(self.pane_heading(
                div().text_sm().text_color(theme.muted_foreground).child(tr!("workbench-select-a-card-to-view-it-here")), cx,
            ).id("workbench-pane-header").px(PANE_INSET).py_1());
        };
        if let Some(editor) = self.editing.as_ref().filter(|editor| editor.path == card.path) {
            return pane.child(self.render_card_file_editor(editor, card, cx));
        }
        let breadcrumb = self.file_browser_supported().then(|| {
            self.render_file_header(&card.path, "card-file-breadcrumb", cx)
        });
        let mut toolbar = h_flex().gap_2().flex_wrap()
            .children((!card.external).then(|| self.card_history_button(format!("card-history-{}", card.id), &card.id, cx.entity().downgrade())).flatten());
        if self.from_pdf && self.pdf.is_some() {
            toolbar = toolbar.child(icon_pill("back-pdf", Symbol::ArrowLeft, tr!("workbench-back-to-pdf"), cx).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.right = RightPane::Pdf;
                    cx.notify();
                }),
            ));
        }
        if let Some(src) = card.source.as_deref().and_then(parse_source) {
            let label = if src.item.is_empty() { tr!("workbench-source-page", page = src.page.to_string()) } else { tr!("workbench-source-page-with-details", page = src.page.to_string(), detail = src.item.to_string()) };
            toolbar = toolbar.child(pill("open-source", label, false, cx).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.open_source(src.pdf.clone(), src.page, window, cx)),
            ));
        }
        let p = card.path.clone();
        toolbar = toolbar.child(pill("edit-card", tr!("workbench-edit"), false, cx).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| this.start_edit(p.clone(), window, cx)),
        ));
        let pane = match breadcrumb {
            Some(breadcrumb) => pane
                .child(self.pane_heading(breadcrumb, cx).id("workbench-pane-header").px(PANE_INSET).pt_1())
                .child(toolbar.px(PANE_INSET).pt_2()),
            None => pane.child(self.pane_heading(toolbar, cx).id("workbench-pane-header").px(PANE_INSET).py_1()),
        };
        let pane = pane
            .when(!card.issues.is_empty(), |pane| pane.child(div().px(PANE_INSET).pt_2().child(self.research_issue_chips(card, cx))))
            .children(self.notice.clone().map(|n| div().px(PANE_INSET).pt_2().text_xs().text_color(theme.muted_foreground).child(n)));
        let link_row = |rel: &str, arrow: Symbol, direction: &str, target: &str, cx: &mut Context<Self>| -> AnyElement {
            let exists = self.cards.iter().any(|c| c.id == target);
            let t = target.to_string();
            let row = div()
                .id(SharedString::from(format!("link-{direction}-{rel}-{target}")))
                .text_xs()
                .child(icon_label(arrow, format!("{rel}  {target}"), theme.muted_foreground))
                .when(exists, |d| {
                    d.text_color(theme.link)
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
                            if this.protocol < fuzukue_proto::SYMBOL_PROTOCOL || !(event.modifiers.control || event.modifiers.platform) {
                                this.select(t.clone(), cx);
                            }
                        }))
                })
                .when(!exists, |d| d.text_color(theme.danger))
                ;
            let lookup = fuzukue_proto::SymbolLookup::Name { deck: self.deck.clone(), name: target.to_owned(), kind: fuzukue_proto::SymbolKind::Card };
            self.symbol_menu(row, &cx.entity(), move |_, _| Some(lookup.clone())).into_any_element()
        };
        let mut links = v_flex().gap_0p5();
        for (rel, to) in &card.links {
            links = links.child(link_row(rel, Symbol::ArrowRight, "out", to, cx));
        }
        for (rel, from) in &card.backlinks {
            links = links.child(link_row(rel, Symbol::ArrowLeft, "in", from, cx));
        }
        let mut files = v_flex().gap_0p5();
        for (index, path) in card.files.iter().enumerate() {
            let target = path.clone();
            files = files.child(
                div().id(SharedString::from(format!("card-file-{index}")))
                    .text_xs()
                    .text_color(theme.link)
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .child(icon_label(Symbol::FileText, path.clone(), theme.muted_foreground))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                        this.open_source_file(target.clone(), None, window, cx);
                    })),
            );
        }
        let mut lean_box = v_flex().gap_0p5();
        for decl in &card.lean {
            let (mark, color, word) = match self.lean.get(decl).map(String::as_str) {
                Some("defined") => (Symbol::Check, theme.success, tr!("workbench-declaration-checked").to_string()),
                Some("proved") if matches!(card.kind.as_str(), "definition" | "introdef") =>
                    (Symbol::Check, theme.success, tr!("workbench-declaration-checked").to_string()),
                Some("proved") => (Symbol::Check, theme.success, tr!("workbench-proved").to_string()),
                Some("sorry") => (Symbol::TriangleAlert, theme.warning, tr!("workbench-contains-sorry").to_string()),
                Some("missing") => (Symbol::X, theme.danger, tr!("workbench-not-found").to_string()),
                Some(other) => (Symbol::TriangleAlert, theme.warning, other.to_string()),
                None => (Symbol::Ellipsis, theme.muted_foreground, if self.lean_running { tr!("workbench-checking").into() } else { tr!("workbench-unchecked").into() }),
            };
            let label = format!("Lean {decl} · {word}");
            let mut row = div().id(SharedString::from(format!("card-lean-{decl}")))
                .text_xs().text_color(color).child(icon_label(mark, label, theme.muted_foreground));
            if self.protocol >= fuzukue_proto::LITERATE_LEAN_PROTOCOL {
                let decl = decl.clone();
                row = row.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    this.open_card_lean(decl.clone(), cx);
                }));
            }
            lean_box = lean_box.child(row);
        }
        let mut diag_box = v_flex().gap_0p5().children(self.translation_diagnostic(card, cx))
            .children(self.card_findings_button(card, "card-detail-review-findings", cx.entity().downgrade(), cx));
        for d in self.card_diags.iter().filter(|d| d.path == card.path && d.style.is_none()) {
            diag_box = diag_box.child(
                div().text_xs().text_color(if d.error { theme.danger } else { theme.warning }).child(card_diagnostic_text(d)),
            );
        }
        let uses_tex = card_reading::uses_tex(card);
        let display = std::sync::Arc::new(self.display_card_detail(card, uses_tex));
        let id = SharedString::from(format!("body-{}-{}", self.deck, card.id));
        let view = window.use_keyed_state(id.clone(), cx, |_, cx| TextViewState::markdown(&display.markdown, cx));
        if view.read(cx).rendered_text().source() != display.markdown {
            view.update(cx, |view, cx| view.set_text(&display.markdown, cx));
        }
        let scroll = self.card_detail_scroll(id.clone(), window, cx);
        let text = self.card_body_text(TextView::new(&view), uses_tex, cx)
            .on_reveal({
                let scroll = scroll.clone();
                move |line, _, _| {
                    let viewport = scroll.bounds();
                    let mut offset = scroll.offset();
                    if line.top() < viewport.top() {
                        offset.y += viewport.top() - line.top();
                    } else if line.bottom() > viewport.bottom() {
                        offset.y -= line.bottom() - viewport.bottom();
                    }
                    scroll.set_offset(offset);
                }
            });
        let style_notes = self.render_card_style_notes(
            card, style_notes::StyleNoteView::Reading(&display, &view), cx,
        );
        let content = div().id("card-detail").size_full().overflow_y_scroll()
            .track_scroll(&scroll).child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .px(PANE_INSET).py_4()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_lg().child(
                                paper::theorem_heading(&card.kind, &card.id, &card.title, self.doc_references.as_ref())
                                    .unwrap_or_else(|| if card.title.is_empty() { card.id.clone() } else { card.title.clone() }),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} · {} · {} · {}", card.id, card.kind, card.status, card.path)),
                            )
                            .children(style_notes),
                    )
                    .children(card.research_origin.as_ref().map(|origin| {
                        self.research_origin_link(origin, format!("card-origin-{}", card.id), cx.entity(), cx)
                    }))
                    .child(diag_box)
                    .children(self.deck_diagnostics_panel(Some(&card.id), cx))
                    .child(lean_box)
                    .child(links)
                    .child(files)
                    .children(self.card_body_status(&card.id, &cx.entity(), cx))
                    .child(self.symbol_menu(
                        div().id(id).child(self.appearance.style_reading(div().w_full()).child(text)),
                        &cx.entity(), symbols::text_lookup(self.deck.clone(), card.id.clone(), card.body.as_str().into(), display, view),
                    )),
            );
        // The scrollbar overlays the viewport, so its track stays fixed as the card scrolls.
        pane.child(
            div().relative().flex_1().min_h_0().w_full()
                .child(content)
                .vertical_scrollbar(&scroll),
        )
    }
}

fn card_diagnostic_text(diagnostic: &CardDiagnostic) -> String {
    match &diagnostic.style {
        Some(style) => i18n::current().style_diagnostic(&style.code, style.textlint_rule.as_deref(), &style.detail),
        None => diagnostic.message.clone(),
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new(updater::HELPER_ARG)) {
        if let Err(err) = updater::helper(args) {
            eprintln!("{}", tr!("distribution-update-error", detail = format!("{err:#}")));
            std::process::exit(1);
        }
        return;
    }
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    if std::env::args().nth(1).as_deref() == Some("--headless-shot") {
        if let Err(err) = shot::main() {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("--configure-wsl") {
            if let Err(err) = appearance::configure_wsl(args.collect()) {
                eprintln!("{}", tr!("workbench-cannot-update-settings", detail = format!("{err:#}")));
                std::process::exit(1);
            }
            return;
        }
    }
    #[cfg(unix)]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--configure-external")) {
        let result = if std::env::args_os().len() == 2 {
            appearance::configure_external()
        } else {
            Err(anyhow::anyhow!("Usage: fuzukue --configure-external"))
        };
        if let Err(err) = result {
            eprintln!("{}", tr!("workbench-cannot-update-settings", detail = format!("{err:#}")));
            std::process::exit(1);
        }
        return;
    }
    updater::offer_renamed_executable();
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        appearance::register_window_typography(cx);
        math::init(cx);
        let options = appearance::main_window_options(cx);
        gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title("fuzukue");
            cx.new(|cx| Workbench::new(window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
        #[cfg(target_os = "macos")]
        show_packaged_setup_guide(cx);
    });
}

#[cfg(target_os = "macos")]
fn show_packaged_setup_guide(cx: &mut gpui_kit::App) {
    let Some(script) = std::env::current_exe().ok().and_then(|exe| {
        Some(exe.parent()?.parent()?.join("Resources/install.sh"))
    }).filter(|script| script.is_file()) else { return };
    cx.background_spawn(async move {
        let _ = std::process::Command::new("/bin/sh")
            .arg(script)
            .arg("--guide")
            .status();
    }).detach();
}
