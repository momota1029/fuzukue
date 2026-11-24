//! Account setup and the editable bug-report preview.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use fuzukue_proto::{
    ClientMsg, GitHubAvatar, GitHubConnectState as State, GitHubReportAction as Action,
    GitHubReportError as Error, GitHubReportStatus as Status, GitHubReportUpdate as Update,
    WorkspaceKind, GITHUB_CONNECT_PROTOCOL, GITHUB_REPORT_PROTOCOL,
};
use gpui_kit::{
    base::StyledExt as _,
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputState, Textarea, TextareaState},
        popover::Popover,
        text::TextView,
        v_flex, ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    },
    div, img,
    prelude::*,
    px, rems, Anchor, ClipboardItem, Context, Div, Entity, Image, ImageFormat, ObjectFit, Rems,
    Task, Window,
};
use url::Url;

use crate::{icons::Symbol, ConnectionStatus, Workbench};

const BROWSER_URL_LIMIT: usize = 2000;

#[derive(Default)]
pub(super) struct GitHubReport {
    status: Option<Status>,
    next: u64,
    pending: Option<(u64, Operation)>,
    account: Option<State>,
    legacy_device: Option<State>,
    timer: Option<Task<()>>,
    avatar: Option<CachedAvatar>,
    menu_open: bool,
    confirm: Option<Disconnect>,
    account_error: Option<Error>,
    draft: Option<Draft>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Status,
    Connect,
    Disconnect(Disconnect),
    Submit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disconnect {
    Remove,
    Switch,
}

struct CachedAvatar {
    login: String,
    etag: String,
    image: Arc<Image>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountSurface {
    Settings,
    Report,
    Menu,
}

impl AccountSurface {
    fn id(self) -> &'static str {
        match self {
            Self::Settings => "github-settings-account",
            Self::Report => "github-report-account",
            Self::Menu => "github-account-menu",
        }
    }
}

struct Draft {
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    screenshot: Option<ReportScreenshot>,
    error: Option<Error>,
    issue: Option<String>,
    omitted: Option<(bool, bool)>,
}

struct ReportScreenshot {
    image: Arc<Image>,
    save: ScreenshotSave,
}

enum ScreenshotSave {
    Ready,
    Saving,
    Saved(PathBuf),
    Failed(String),
}

struct BrowserLink {
    url: Url,
    omitted_title: bool,
    omitted_body: bool,
}

fn browser_link(repo: &str, title: &str, body: &str) -> Option<BrowserLink> {
    let (owner, name) = repo.split_once('/')?;
    if ![owner, name].into_iter().all(|part| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return None;
    }

    let base = Url::parse(&format!("https://github.com/{repo}/issues/new")).ok()?;
    let mut url = base.clone();
    url.query_pairs_mut()
        .append_pair("title", title)
        .append_pair("body", body);
    if url.as_str().len() <= BROWSER_URL_LIMIT {
        return Some(BrowserLink {
            url,
            omitted_title: false,
            omitted_body: false,
        });
    }
    let mut url = base.clone();
    url.query_pairs_mut().append_pair("title", title);
    let omitted_title = url.as_str().len() > BROWSER_URL_LIMIT;
    Some(BrowserLink {
        url: if omitted_title { base } else { url },
        omitted_title,
        omitted_body: true,
    })
}

fn template(protocol: u32, kind: Option<WorkspaceKind>) -> String {
    let unknown = tr!("github-report-unknown");
    let revision = env!("FUZUKUE_CLIENT_BUILD_ID");
    let kind = match kind {
        Some(WorkspaceKind::Research) => tr!("github-report-kind-research"),
        Some(WorkspaceKind::Paper) => tr!("github-report-kind-paper"),
        Some(WorkspaceKind::Software) => tr!("github-report-kind-software"),
        Some(WorkspaceKind::Empty) => tr!("github-report-kind-empty"),
        None => unknown,
    };
    tr!(
        "github-report-template",
        version = env!("CARGO_PKG_VERSION"),
        revision = if revision.is_empty() {
            unknown
        } else {
            revision
        },
        os = std::env::consts::OS,
        protocol = protocol.to_string(),
        kind = kind
    )
}

fn error_text(error: Error) -> String {
    match error {
        Error::Disabled => tr!("github-report-disabled"),
        Error::Config => tr!("github-report-config-error"),
        Error::Storage => tr!("github-report-storage-error"),
        Error::Offline => tr!("github-report-offline"),
        Error::Rejected(code) => return tr!("github-report-rejected", code = code.to_string()),
        Error::Response => tr!("github-report-response-error"),
        Error::Denied => tr!("github-report-denied"),
        Error::Expired => tr!("github-report-expired"),
        Error::Cancelled => tr!("github-report-cancelled"),
        Error::AlreadyConnected => tr!("github-report-already-connected"),
        Error::NotConnected => tr!("github-report-not-connected"),
        Error::AccountChanged => tr!("github-report-account-changed"),
        Error::Repository => tr!("github-report-repo-denied"),
        Error::EmptyReport => tr!("github-report-empty"),
    }
    .into()
}

impl Workbench {
    fn github_report_repo(&self) -> Option<&str> {
        if self.protocol < GITHUB_REPORT_PROTOCOL {
            return browser_link(&self.repo, "", "").map(|_| self.repo.as_str());
        }
        self.github_report.status.as_ref()?.report_repo.as_deref()
    }

    fn github_report_login(&self) -> Option<&str> {
        match &self.github_report.account {
            Some(State::Connected { login }) => Some(login),
            Some(_) => None,
            None => self.github_report.status.as_ref()?.login.as_deref(),
        }
    }

    fn github_report_connected(&self) -> bool {
        matches!(self.status, ConnectionStatus::Connected)
            && self.protocol >= GITHUB_REPORT_PROTOCOL
    }

    pub(super) fn github_account_changed(&mut self, state: State, cx: &mut Context<Self>) {
        self.github_report.account_error = None;
        self.github_report.confirm = None;
        if matches!(state, State::Pending { .. }) {
            self.start_github_countdown(cx);
        } else {
            self.github_report.timer = None;
        }
        self.github_report.account = Some(state);
        cx.notify();
    }

    pub(super) fn github_avatar(&mut self, avatar: GitHubAvatar, cx: &mut Context<Self>) {
        let report = &mut self.github_report;
        if report
            .avatar
            .as_ref()
            .is_some_and(|cached| cached.login == avatar.login && cached.etag == avatar.etag)
            || avatar.png.len() > (256 * 1024 * 4 / 3 + 4)
        {
            return;
        }
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(avatar.png) else {
            return;
        };
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return;
        }
        report.avatar = Some(CachedAvatar {
            login: avatar.login,
            etag: avatar.etag,
            image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
        });
        cx.notify();
    }

    fn start_github_countdown(&mut self, cx: &mut Context<Self>) {
        if self.github_report.timer.is_some() {
            return;
        }
        self.github_report.timer = Some(cx.spawn(async move |view, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if view.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }));
    }

    pub(super) fn refresh_github_report_status(&mut self, cx: &mut Context<Self>) {
        if self.github_report_connected() {
            self.github_report_request(Action::Status, Operation::Status, cx);
        }
    }

    fn github_report_request(
        &mut self,
        action: Action,
        operation: Operation,
        cx: &mut Context<Self>,
    ) {
        if self.github_report.pending.is_some() {
            return;
        }
        let report = &mut self.github_report;
        report.next += 1;
        let request = report.next;
        report.pending = Some((request, operation));
        if operation != Operation::Status {
            report.account_error = None;
        }
        if self
            .out
            .send(ClientMsg::GitHubReport { request, action })
            .is_err()
        {
            self.github_report_result(request, Err(Error::Offline), cx);
        }
        cx.notify();
    }

    pub(super) fn github_report_result(
        &mut self,
        request: u64,
        result: Result<Update, Error>,
        cx: &mut Context<Self>,
    ) {
        let report = &mut self.github_report;
        let operation = match report.pending {
            Some((expected, operation)) if expected == request => Some(operation),
            None if request == 0 => None, // Initial workspace status.
            _ => return,
        };
        match result {
            Ok(Update::Device {
                user_code,
                verification_uri,
                expires_at,
            }) => {
                if self.protocol < GITHUB_CONNECT_PROTOCOL {
                    report.legacy_device = Some(State::Pending {
                        code: user_code,
                        url: verification_uri,
                        expires_at,
                    });
                    self.start_github_countdown(cx);
                }
            }
            result => {
                report.legacy_device = None;
                if self.protocol < GITHUB_CONNECT_PROTOCOL {
                    report.timer = None;
                }
                report.pending = None;
                match result {
                    Ok(Update::Status { status }) => {
                        report.status = Some(status);
                        report.account_error = None;
                        if operation == Some(Operation::Disconnect(Disconnect::Switch)) {
                            self.github_report_request(Action::Connect, Operation::Connect, cx);
                        }
                    }
                    Ok(Update::Submitted { url }) => {
                        if let Some(draft) = &mut report.draft {
                            draft.issue = Some(url);
                        }
                    }
                    Err(error) => {
                        if operation == Some(Operation::Submit) {
                            if let Some(draft) = &mut report.draft {
                                draft.error = Some(error);
                            }
                        } else {
                            report.account_error = Some(error);
                        }
                        if matches!(
                            error,
                            Error::AccountChanged
                                | Error::AlreadyConnected
                                | Error::NotConnected
                                | Error::Repository
                                | Error::Disabled
                        ) && operation != Some(Operation::Status)
                        {
                            report.status = None;
                            self.refresh_github_report_status(cx);
                        }
                    }
                    Ok(Update::Device { .. }) => unreachable!(),
                }
            }
        }
        cx.notify();
    }

    pub(super) fn github_report_disconnected(&mut self, cx: &mut Context<Self>) {
        if let Some((request, _)) = self.github_report.pending {
            self.github_report_result(request, Err(Error::Offline), cx);
        }
        self.github_report.account = None;
        self.github_report.legacy_device = None;
        self.github_report.timer = None;
        self.github_report.confirm = None;
        // Keep the last-known repository for the browser fallback.
        if let Some(status) = &mut self.github_report.status {
            status.login = None;
        }
        cx.notify();
    }

    pub(super) fn clear_github_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.github_report.draft.take().is_some() {
            window.close_dialog(cx);
        }
        self.github_report = GitHubReport {
            next: self.github_report.next,
            ..Default::default()
        };
    }

    pub(super) fn open_github_report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_github_report_draft(None, window, cx);
    }

    pub(super) fn open_github_report_draft(
        &mut self,
        screenshot: Option<(Arc<Image>, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if (screenshot.is_some() && window.has_active_dialog(cx))
            || self.github_report.draft.is_some()
            || self
                .github_report
                .pending
                .is_some_and(|(_, op)| op == Operation::Submit)
        {
            return false;
        }
        let body = template(
            self.protocol,
            self.github_report
                .status
                .as_ref()
                .map(|status| status.workspace_kind),
        );
        let mut title = format!("{} ", tr!("github-report-title-prefix"));
        let (screenshot, body) = match screenshot {
            Some((image, note)) => {
                title.push_str(tr!("github-report-screenshot-title"));
                let body = if note.is_empty() {
                    body
                } else {
                    format!("{note}\n\n{body}")
                };
                (
                    Some(ReportScreenshot {
                        image,
                        save: ScreenshotSave::Ready,
                    }),
                    body,
                )
            }
            None => (None, body),
        };
        self.github_report.draft = Some(Draft {
            title: cx.new(|cx| InputState::new(window, cx).default_value(title)),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(5, 10)
                    .default_value(body)
            }),
            screenshot,
            error: None,
            issue: None,
            omitted: None,
        });

        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let height = window.viewport_size().height * 0.72;
            let Ok(content) = view.update(cx, |this, cx| this.render_github_report_dialog(cx))
            else {
                return dialog;
            };
            let close_view = view.clone();
            let cancel_view = view.clone();
            dialog
                .w(px(640.))
                .title(tr!("github-report-menu"))
                .overlay_closable(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, cx| {
                    cancel_view
                        .update(cx, |this, _| {
                            !this
                                .github_report
                                .pending
                                .is_some_and(|(_, op)| op == Operation::Submit)
                        })
                        .unwrap_or(true)
                })
                .on_close(move |_, _, cx| {
                    let _ = close_view.update(cx, |this, cx| {
                        this.github_report.draft = None;
                        cx.notify();
                    });
                })
                .child(
                    div()
                        .id("github-report-dialog")
                        .w_full()
                        .max_h(height)
                        .overflow_y_scroll()
                        .child(content),
                )
        });
        if self.github_report.status.is_none() {
            self.refresh_github_report_status(cx);
        }
        true
    }

    fn submit_github_report(&mut self, cx: &mut Context<Self>) {
        if !self.github_report_connected() || self.github_report.pending.is_some() {
            return;
        }
        let Some((repo, login)) = self.github_report_repo().zip(self.github_report_login()) else {
            return;
        };
        let (repo, login) = (repo.to_owned(), login.to_owned());
        let Some(draft) = &mut self.github_report.draft else {
            return;
        };
        if draft.issue.is_some() {
            return;
        }
        let title = draft.title.read(cx).value().to_string();
        let body = draft.body.read(cx).value().to_string();
        if title.trim().is_empty()
            || title.trim() == tr!("github-report-title-prefix")
            || body.trim().is_empty()
        {
            draft.error = Some(Error::EmptyReport);
            cx.notify();
            return;
        }
        draft.error = None;
        self.github_report_request(
            Action::Submit {
                repo,
                login,
                title,
                body,
            },
            Operation::Submit,
            cx,
        );
    }

    fn open_github_report_browser(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.github_report_repo().map(str::to_owned) else {
            return;
        };
        let Some(draft) = &mut self.github_report.draft else {
            return;
        };
        if let Some(link) = browser_link(
            &repo,
            &draft.title.read(cx).value(),
            &draft.body.read(cx).value(),
        ) {
            draft.omitted = (link.omitted_title || link.omitted_body)
                .then_some((link.omitted_title, link.omitted_body));
            cx.open_url(link.url.as_str());
            cx.notify();
        }
    }

    pub(super) fn render_github_settings(&self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(tr!("github-report-settings")),
            )
            .child(self.render_github_account(AccountSurface::Settings, cx))
    }

    pub(super) fn render_github_title_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let report = &self.github_report;
        let login = self.github_report_login();
        let tooltip = self.github_account_label();
        let connected = login.is_some();
        let pending = self.github_connect_pending();
        let icon_size = rems(0.875);
        let mark = self.github_account_mark("github-title-mark", login, icon_size, cx);
        let theme = cx.theme();
        let dot_color = if connected {
            theme.success
        } else if pending {
            theme.warning
        } else {
            theme.muted_foreground
        };
        let dot_id = if connected {
            "github-title-connected"
        } else if pending {
            "github-title-connecting"
        } else {
            "github-title-disconnected"
        };
        let view = cx.weak_entity();
        Popover::new("github-account-popover")
            .anchor(Anchor::BottomRight)
            .offset(px(4.))
            .open(report.menu_open)
            .trigger(
                Button::new("github-title-button")
                    .ghost()
                    .small()
                    .compact()
                    .size_6()
                    .p_0()
                    .accessibility_label(tooltip.clone())
                    .tooltip(tooltip)
                    .child(div().relative().size(icon_size).child(mark).when(
                        !self.has_github_avatar(login),
                        |mark| {
                            mark.child(
                                div()
                                    .id(dot_id)
                                    .absolute()
                                    .bottom(px(-1.))
                                    .right(px(-2.))
                                    .size(px(6.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(theme.title_bar)
                                    .bg(dot_color),
                            )
                        },
                    )),
            )
            .on_open_change(cx.listener(|this, &open, _, cx| {
                this.github_report.menu_open = open;
                if !open {
                    this.github_report.confirm = None;
                }
                cx.notify();
            }))
            .content(move |_, window, cx| {
                view.update(cx, |this, cx| {
                    let theme = cx.theme();
                    let width = (window.viewport_size().width - px(24.)).clamp(px(210.), px(360.));
                    let login = this.github_report_login();
                    v_flex()
                        .w(width)
                        .p_3()
                        .gap_2()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .rounded_md()
                        .shadow_md()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(this.github_account_mark(
                                    "github-menu-mark",
                                    login,
                                    rems(1.5),
                                    cx,
                                ))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_semibold()
                                        .child(login.unwrap_or("GitHub").to_owned()),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(this.github_account_label()),
                        )
                        .child(this.render_github_account(AccountSurface::Menu, cx))
                })
                .unwrap_or_else(|_| v_flex())
            })
    }

    fn has_github_avatar(&self, login: Option<&str>) -> bool {
        self.github_report
            .avatar
            .as_ref()
            .is_some_and(|avatar| Some(avatar.login.as_str()) == login)
    }

    fn github_account_mark(
        &self,
        id: &'static str,
        login: Option<&str>,
        size: Rems,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        if let Some(avatar) = &self.github_report.avatar {
            if Some(avatar.login.as_str()) == login {
                return div()
                    .id(format!("{id}-avatar"))
                    .size(size)
                    .flex_none()
                    .rounded_full()
                    .overflow_hidden()
                    .child(
                        img(avatar.image.clone())
                            .size_full()
                            .rounded_full()
                            .object_fit(ObjectFit::Cover),
                    );
            }
        }
        div().id(id).size(size).flex_none().child(
            Symbol::GitBranch
                .icon()
                .size(size)
                .text_color(cx.theme().foreground),
        )
    }

    fn github_connect_pending(&self) -> bool {
        self.github_report
            .pending
            .is_some_and(|(_, op)| op == Operation::Connect)
            || matches!(self.github_report.account, Some(State::Pending { .. }))
            || self.github_report.legacy_device.is_some()
    }

    fn github_account_label(&self) -> String {
        if self.github_connect_pending() {
            tr!("github-auth-pending").into()
        } else if let Some(login) = self.github_report_login() {
            tr!("github-auth-title-connected", login = login)
        } else {
            tr!("github-auth-title-disconnected").into()
        }
    }

    fn render_github_account(&self, surface: AccountSurface, cx: &mut Context<Self>) -> Div {
        let report = &self.github_report;
        let menu = surface == AccountSurface::Menu;
        let theme = cx.theme().clone();
        let mut content = v_flex().gap_2().text_sm();
        let state = report.account.as_ref().or(report.legacy_device.as_ref());
        let connecting = report
            .pending
            .is_some_and(|(_, op)| op == Operation::Connect);
        let enabled = match state {
            Some(State::NotConfigured) => false,
            Some(State::Failed { .. }) => {
                report.status.as_ref().is_some_and(|status| status.enabled)
            }
            Some(_) => true,
            None => report.status.as_ref().is_some_and(|status| status.enabled),
        };
        let disabled = report.pending.is_some() || !self.github_report_connected();
        if let Some(state @ State::Pending { .. }) = state {
            content = content.child(render_github_connect_card(surface.id(), state, false, cx));
        } else if connecting {
            content = content.child(tr!("github-report-starting"));
        } else if let Some(login) = self.github_report_login() {
            if !menu {
                content = content.child(tr!("github-report-connected", login = login));
            }
            if let Some(action) = report.confirm {
                let summary = match action {
                    Disconnect::Remove => tr!("github-auth-confirm-disconnect"),
                    Disconnect::Switch => tr!("github-auth-confirm-switch"),
                };
                content = content.child(summary).child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("github-disconnect-confirm")
                                .danger()
                                .label(tr!("github-report-disconnect"))
                                .disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.github_report.confirm = None;
                                    this.github_report_request(
                                        Action::Disconnect,
                                        Operation::Disconnect(action),
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("github-disconnect-cancel")
                                .label(tr!("github-auth-cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.github_report.confirm = None;
                                    cx.notify();
                                })),
                        ),
                );
            } else {
                let button = |id, label, action| {
                    Button::new(id)
                        .label(label)
                        .disabled(disabled)
                        .when(menu, |button| button.ghost().w_full().justify_start())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.github_report.confirm = Some(action);
                            cx.notify();
                        }))
                };
                if menu {
                    content = content
                        .child(
                            Button::new("github-menu-report")
                                .ghost()
                                .w_full()
                                .justify_start()
                                .label(tr!("github-auth-report"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.github_report.menu_open = false;
                                    this.open_github_report(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(button(
                            "github-menu-switch",
                            tr!("github-auth-switch"),
                            Disconnect::Switch,
                        ));
                }
                content = content.child(button(
                    "github-disconnect",
                    tr!("github-report-disconnect"),
                    Disconnect::Remove,
                ));
            }
        } else if enabled {
            let failure = report.account_error.or(match state {
                Some(State::Denied) => Some(Error::Denied),
                Some(State::Expired) => Some(Error::Expired),
                Some(State::Failed { error }) => Some(*error),
                _ => None,
            });
            if !menu && failure.is_none() {
                content = content.child(
                    div()
                        .id("github-report-account-disconnected")
                        .child(tr!("github-report-disconnected")),
                );
            }
            content = content
                .child(
                    Button::new("github-connect")
                        .label(if failure.is_some() {
                            tr!("github-auth-retry")
                        } else {
                            tr!("github-auth-connect")
                        })
                        .when(menu, |button| button.ghost().w_full().justify_start())
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.github_report_request(Action::Connect, Operation::Connect, cx);
                        })),
                )
                .children(
                    failure.map(|error| div().text_color(theme.danger).child(error_text(error))),
                );
            return content;
        } else if report.pending.is_some()
            || (self.github_report_connected()
                && report.status.is_none()
                && state.is_none()
                && report.account_error.is_none())
        {
            content = content.child(tr!("github-report-loading"));
        } else {
            content = content.child(tr!("github-report-disabled"));
        }
        content.children(
            report
                .account_error
                .map(|error| div().text_color(theme.danger).child(error_text(error))),
        )
    }

    fn save_report_screenshot(&mut self, cx: &mut Context<Self>) {
        let Some(screenshot) = self
            .github_report
            .draft
            .as_mut()
            .and_then(|draft| draft.screenshot.as_mut())
        else {
            return;
        };
        if matches!(screenshot.save, ScreenshotSave::Saving) {
            return;
        }
        screenshot.save = ScreenshotSave::Saving;
        let image = screenshot.image.clone();
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| ".".into());
        let picker = cx.prompt_for_new_path(&dir, Some("fuzukue-screenshot.png"));
        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<Option<PathBuf>> = async {
                let Some(path) = picker.await?? else {
                    return Ok(None);
                };
                let image = image.clone();
                cx.background_executor()
                    .spawn(async move {
                        let dir = path.parent().unwrap_or_else(|| Path::new("."));
                        let mut file = tempfile::NamedTempFile::new_in(dir)?;
                        file.write_all(image.bytes())?;
                        file.persist(&path)?;
                        Ok(Some(path))
                    })
                    .await
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(screenshot) = this
                    .github_report
                    .draft
                    .as_mut()
                    .and_then(|draft| draft.screenshot.as_mut())
                    .filter(|screenshot| Arc::ptr_eq(&screenshot.image, &image))
                {
                    screenshot.save = match result {
                        Ok(Some(path)) => ScreenshotSave::Saved(path),
                        Ok(None) => ScreenshotSave::Ready,
                        Err(err) => ScreenshotSave::Failed(err.to_string()),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn render_report_screenshot(
        screenshot: &ReportScreenshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status = match &screenshot.save {
            ScreenshotSave::Ready | ScreenshotSave::Saving => None,
            ScreenshotSave::Saved(path) => Some(div().child(tr!(
                "github-report-screenshot-saved",
                path = path.display().to_string()
            ))),
            ScreenshotSave::Failed(error) => Some(div().text_color(cx.theme().danger).child(tr!(
                "github-report-screenshot-save-error",
                error = error.as_str()
            ))),
        };
        let saving = matches!(screenshot.save, ScreenshotSave::Saving);
        h_flex()
            .id("github-report-screenshot")
            .gap_3()
            .child(
                img(screenshot.image.clone())
                    .w(px(112.))
                    .h(px(72.))
                    .flex_none()
                    .object_fit(ObjectFit::Contain),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .items_start()
                    .gap_1()
                    .text_sm()
                    .child(tr!("github-report-screenshot-attach"))
                    .child(
                        Button::new("github-report-save-screenshot")
                            .label(if saving {
                                tr!("screenshot-saving")
                            } else {
                                tr!("github-report-save-screenshot")
                            })
                            .disabled(saving)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.save_report_screenshot(cx)),
                            ),
                    )
                    .children(status),
            )
    }

    fn render_github_report_dialog(&self, cx: &mut Context<Self>) -> Div {
        let Some(draft) = &self.github_report.draft else {
            return div();
        };
        let theme = cx.theme().clone();
        let screenshot = draft
            .screenshot
            .as_ref()
            .map(|screenshot| Self::render_report_screenshot(screenshot, cx));
        if let Some(url) = &draft.issue {
            let url = url.clone();
            return v_flex()
                .gap_2()
                .child(tr!("github-report-sent"))
                .child(
                    Button::new("github-report-issue-url")
                        .link()
                        .label(url.clone())
                        .on_click(move |_, _, cx| {
                            if url.starts_with("https://github.com/") {
                                cx.open_url(&url);
                            }
                        }),
                )
                .children(screenshot);
        }
        let repo = self.github_report_repo();
        let sending = self
            .github_report
            .pending
            .is_some_and(|(_, op)| op == Operation::Submit);
        let can_send = self.github_report_connected() && self.github_report_login().is_some();
        let mut actions = h_flex().gap_2().flex_wrap();
        if can_send {
            actions = actions.child(
                Button::new("github-report-send")
                    .primary()
                    .label(if sending {
                        tr!("github-report-sending")
                    } else if draft.screenshot.is_some() {
                        tr!("github-report-send-without-screenshot")
                    } else {
                        tr!("github-report-send")
                    })
                    .disabled(repo.is_none() || self.github_report.pending.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.submit_github_report(cx))),
            );
        }
        if repo.is_some() && (!can_send || draft.screenshot.is_some()) {
            actions = actions.child(
                Button::new("github-report-browser")
                    .label(tr!("github-report-browser"))
                    .disabled(draft.screenshot.is_some() && sending)
                    .on_click(cx.listener(|this, _, _, cx| this.open_github_report_browser(cx))),
            );
        }
        let missing = draft.omitted.map(|(title, body)| {
            let title_input = draft.title.clone();
            let body_input = draft.body.clone();
            v_flex()
                .gap_2()
                .text_sm()
                .child(tr!("github-report-long-body"))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(title.then(|| {
                            Button::new("github-report-copy-title")
                                .label(tr!("github-report-copy-title"))
                                .on_click(move |_, _, cx| {
                                    let title = title_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(title));
                                })
                        }))
                        .children(body.then(|| {
                            Button::new("github-report-copy-body")
                                .label(tr!("github-report-copy-body"))
                                .on_click(move |_, _, cx| {
                                    let body = body_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(body));
                                })
                        })),
                )
        });
        v_flex()
            .gap_3()
            .child(
                div()
                    .id("github-report-destination")
                    .text_sm()
                    .child(repo.map_or_else(
                        || tr!("github-report-no-repo").into(),
                        |repo| tr!("github-report-repo", repo = repo),
                    )),
            )
            .child(self.render_github_account(AccountSurface::Report, cx))
            .children(
                (draft.screenshot.is_some() && !matches!(self.status, ConnectionStatus::Connected))
                    .then(|| {
                        div()
                            .id("github-report-server-required")
                            .text_sm()
                            .text_color(theme.danger)
                            .child(tr!("github-report-server-required"))
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("github-report-preview")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(tr!("github-report-title")))
                    .child(
                        div().id("github-report-title-input").child(
                            Input::new(&draft.title)
                                .aria_label(tr!("github-report-title"))
                                .disabled(sending),
                        ),
                    ),
            )
            .children(screenshot)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(tr!("github-report-body")))
                    .child(
                        div().id("github-report-body-input").child(
                            Textarea::new(&draft.body)
                                .aria_label(tr!("github-report-body"))
                                .disabled(sending),
                        ),
                    ),
            )
            .children(draft.error.map(|error| {
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error_text(error))
            }))
            .child(actions)
            .children(missing)
    }
}

pub(super) fn render_github_connect_card(
    id: &str,
    state: &State,
    chat: bool,
    cx: &gpui_kit::App,
) -> Div {
    let theme = cx.theme();
    let mut card = v_flex().min_w_0().gap_2().text_sm();
    if chat {
        if let State::Connected { login } = state {
            return card.child(
                div()
                    .id("github-chat-connected")
                    .text_color(theme.success)
                    .child(tr!("github-auth-chat-connected", login = login)),
            );
        }
        card = card
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(div().font_semibold().child(tr!("github-auth-title")));
    }
    match state {
        State::Pending {
            code,
            url,
            expires_at,
        } => {
            let seconds = expires_at
                .saturating_sub(chrono::Utc::now().timestamp())
                .max(0);
            let remaining = format!("{:02}:{:02}", seconds / 60, seconds % 60);
            let code_copy = code.clone();
            let url_copy = url.clone();
            let browser_url = url.clone();
            card.child(
                div()
                    .id(format!("{id}-pending"))
                    .text_color(theme.muted_foreground)
                    .child(tr!("github-auth-countdown", remaining = remaining)),
            )
            .child(TextView::markdown(format!("{id}-code"), format!("`{code}`")).selectable(true))
            .child(TextView::markdown(format!("{id}-url"), format!("`{url}`")).selectable(true))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(format!("{id}-copy-code"))
                            .small()
                            .label(tr!("github-report-copy-code"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(code_copy.clone()))
                            }),
                    )
                    .child(
                        Button::new(format!("{id}-copy-url"))
                            .small()
                            .label(tr!("github-auth-copy-url"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url_copy.clone()))
                            }),
                    )
                    .child(
                        Button::new(format!("{id}-open"))
                            .small()
                            .label(tr!("github-report-browser"))
                            .on_click(move |_, _, cx| {
                                if browser_url == "https://github.com/login/device" {
                                    cx.open_url(&browser_url);
                                }
                            }),
                    ),
            )
        }
        State::Denied => card.child(error_text(Error::Denied)),
        State::Expired => card.child(
            div()
                .id("github-chat-expired")
                .child(error_text(Error::Expired)),
        ),
        State::Failed { error } => card.child(error_text(*error)),
        _ => card,
    }
}

#[cfg(test)]
mod tests;
