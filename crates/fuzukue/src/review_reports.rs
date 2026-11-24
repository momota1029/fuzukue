use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine};
use gpui_kit::{
    component::{
        h_flex,
        input::{Input, InputState, Textarea, TextareaState},
        scroll::ScrollableElement as _,
        v_flex, ActiveTheme as _,
    },
    div,
    prelude::*,
    px, Context, Entity, Focusable as _, FontWeight, PathPromptOptions, ScrollHandle, Window,
};
use fuzukue_proto::{
    ClientMsg, ReviewPoint, ReviewPointDraft, ReviewPointStatus, ReviewPointUpdate, ReviewReport,
    ReviewReportSource, ReviewReportsAction, ReviewSplit, ReviewSplitState, ReviewSubmission,
    SubmissionMetadata, MAX_REVIEW_REPORT_FILE_BYTES, REVIEW_REPORTS_PROTOCOL,
};

use super::{pill, Workbench};

#[derive(Default)]
pub(super) struct Pane {
    decks: BTreeMap<String, DeckReviews>,
    open: Option<String>,
    selection: Option<Selection>,
    filter: Option<ReviewPointStatus>,
    proposal: Option<(String, u32, Arc<Vec<ReviewPointDraft>>)>,
    form: Option<Form>,
    saving: Option<u64>,
    next_request: u64,
    error: bool,
    scroll: ScrollHandle,
}

#[derive(Default)]
struct DeckReviews {
    revision: u64,
    submissions: Vec<ReviewSubmission>,
    splits: Vec<ReviewSplit>,
    fixing: Vec<String>,
}

#[derive(Clone)]
enum Section {
    Points,
    Report(u32),
    Point(String),
    Response,
}

struct Selection {
    submission: String,
    section: Section,
}

#[derive(Clone, Copy)]
enum FormKind {
    Create,
    Paste,
    Draft(Option<usize>),
    Point,
}

enum Field {
    Line(&'static str, Entity<InputState>),
    Area(&'static str, Entity<TextareaState>),
}

impl Field {
    fn get(&self, cx: &gpui_kit::App) -> String {
        match self {
            Self::Line(_, state) => state.read(cx).value().to_string(),
            Self::Area(_, state) => state.read(cx).value().to_string(),
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut gpui_kit::App) {
        match self {
            Self::Line(_, state) => state.read(cx).focus_handle(cx).focus(window, cx),
            Self::Area(_, state) => state.read(cx).focus_handle(cx).focus(window, cx),
        }
    }

    fn render(&self, index: usize) -> impl IntoElement {
        let (label, input) = match self {
            Self::Line(label, state) => (
                *label,
                Input::new(state).aria_label(*label).into_any_element(),
            ),
            Self::Area(label, state) => (
                *label,
                Textarea::new(state).aria_label(*label).into_any_element(),
            ),
        };
        v_flex()
            .gap_1()
            .child(div().text_sm().child(label))
            .child(div().id(format!("review-field-{index}")).child(input))
    }
}

struct Form {
    kind: FormKind,
    fields: Vec<Field>,
}

const STATUSES: [ReviewPointStatus; 5] = [
    ReviewPointStatus::Todo,
    ReviewPointStatus::InProgress,
    ReviewPointStatus::Fixed,
    ReviewPointStatus::Rebut,
    ReviewPointStatus::WontFix,
];

fn status_label(status: ReviewPointStatus) -> &'static str {
    match status {
        ReviewPointStatus::Todo => tr!("review-todo"),
        ReviewPointStatus::InProgress => tr!("review-in-progress"),
        ReviewPointStatus::Fixed => tr!("review-fixed"),
        ReviewPointStatus::Rebut => tr!("review-rebut"),
        ReviewPointStatus::WontFix => tr!("review-wont-fix"),
    }
}

fn card_ids(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Workbench {
    pub(super) fn review_reports_available(&self) -> bool {
        self.protocol >= REVIEW_REPORTS_PROTOCOL && self.deck.starts_with("papers/")
    }

    pub(super) fn reviews_visible(&self) -> bool {
        self.review_reports_available() && self.reviews.open.as_ref() == Some(&self.deck)
    }

    pub(super) fn load_review_reports(&self, deck: &str) {
        if self.protocol >= REVIEW_REPORTS_PROTOCOL && deck.starts_with("papers/") {
            let _ = self.out.send(ClientMsg::ReviewReports {
                deck: deck.into(),
                request: None,
                action: ReviewReportsAction::List,
            });
        }
    }

    pub(super) fn on_review_reports(
        &mut self,
        deck: String,
        revision: u64,
        submissions: Vec<ReviewSubmission>,
        splits: Vec<ReviewSplit>,
        fixing: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .reviews
            .decks
            .get(&deck)
            .is_some_and(|previous| previous.revision > revision)
        {
            return;
        }
        self.reviews.decks.insert(
            deck.clone(),
            DeckReviews {
                revision,
                submissions,
                splits,
                fixing,
            },
        );
        if self.deck == deck {
            self.read_list
                .remeasure_items(0..self.read_list.item_count());
        }
        cx.notify();
    }

    pub(super) fn on_review_result(&mut self, deck: &str, request: Option<u64>, ok: bool) {
        if self.deck != deck {
            return;
        }
        if let Some(request) = request {
            if self.reviews.saving != Some(request) {
                return;
            }
            self.reviews.saving = None;
            if ok {
                self.reviews.form = None;
                self.reviews.proposal = None;
            }
        }
        self.reviews.error = !ok;
    }

    pub(super) fn close_reviews(&mut self) {
        self.reviews
            .scroll
            .set_offset(gpui_kit::point(px(0.), px(0.)));
        self.reviews.open = None;
        self.reviews.form = None;
        self.reviews.proposal = None;
        self.reviews.saving = None;
        self.reviews.error = false;
    }

    pub(super) fn open_reviews(
        &mut self,
        deck: String,
        target: Option<(String, u32, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.close_reviews();
        self.reviews.open = Some(deck.clone());
        self.reviews.filter = None;
        self.reviews.selection = target.map(|(submission, report, point)| Selection {
            submission,
            section: point.map_or(Section::Report(report), Section::Point),
        });
        self.load_review_reports(&deck);
        cx.notify();
    }

    fn select_review(&mut self, submission: String, section: Section, cx: &mut Context<Self>) {
        self.reviews
            .scroll
            .set_offset(gpui_kit::point(px(0.), px(0.)));
        self.reviews.selection = Some(Selection {
            submission,
            section,
        });
        self.reviews.form = None;
        self.reviews.proposal = None;
        self.reviews.saving = None;
        self.reviews.error = false;
        cx.notify();
    }

    fn send_review(&mut self, action: ReviewReportsAction, cx: &mut Context<Self>) {
        self.dispatch_review(action, None, cx);
    }

    fn dispatch_review(
        &mut self,
        action: ReviewReportsAction,
        request: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.reviews.error = self
            .out
            .send(ClientMsg::ReviewReports {
                deck: self.deck.clone(),
                request,
                action,
            })
            .is_err();
        if self.reviews.error {
            self.reviews.saving = None;
        }
        cx.notify();
    }

    fn save_review(&mut self, action: ReviewReportsAction, cx: &mut Context<Self>) {
        self.reviews.next_request = self.reviews.next_request.wrapping_add(1);
        let request = Some(self.reviews.next_request);
        self.reviews.saving = request;
        self.dispatch_review(action, request, cx);
    }

    fn review_form(
        &mut self,
        kind: FormKind,
        point: Option<ReviewPoint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut line = |label: &'static str, value: &str, window: &mut Window| {
            Field::Line(
                label,
                cx.new(|cx| InputState::new(window, cx).default_value(value.to_owned())),
            )
        };
        let fields = match kind {
            FormKind::Create => [
                tr!("review-venue"),
                tr!("review-revision"),
                tr!("review-date"),
                tr!("review-decision"),
                tr!("review-deadline"),
            ]
            .into_iter()
            .map(|label| line(label, "", window))
            .collect(),
            FormKind::Draft(index) => {
                let draft = index.and_then(|index| self.reviews.proposal.as_ref()?.2.get(index));
                let range = draft
                    .map(|draft| format!("{}-{}", draft.lines[0], draft.lines[1]))
                    .unwrap_or_default();
                let cards = draft
                    .map(|draft| draft.cards.join(", "))
                    .unwrap_or_default();
                vec![
                    line(tr!("review-lines"), &range, window),
                    line(
                        tr!("review-summary"),
                        draft.map_or("", |draft| &draft.summary),
                        window,
                    ),
                    line(tr!("review-card-ids"), &cards, window),
                ]
            }
            FormKind::Paste => vec![review_area(
                tr!("review-paste-text"),
                String::new(),
                window,
                cx,
            )],
            FormKind::Point => {
                let Some(point) = point else { return };
                vec![
                    line(tr!("review-card-ids"), &point.cards.join(", "), window),
                    review_area(tr!("review-response"), point.response, window, cx),
                    review_area(
                        tr!("review-locations"),
                        point.fixed_locations.join("\n"),
                        window,
                        cx,
                    ),
                ]
            }
        };
        self.reviews
            .scroll
            .set_offset(gpui_kit::point(px(0.), px(0.)));
        if let Some(first) = fields.first() {
            first.focus(window, cx);
        }
        self.reviews.form = Some(Form { kind, fields });
        self.reviews.error = false;
        cx.notify();
    }

    fn submit_review_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.reviews.form else {
            return;
        };
        if self.reviews.saving.is_some() {
            return;
        }
        let values = form
            .fields
            .iter()
            .map(|field| field.get(cx))
            .collect::<Vec<_>>();
        let selected = self.reviews.selection.as_ref();
        let action = match (form.kind, values.as_slice()) {
            (FormKind::Create, [venue, revision, submitted, decision, deadline]) => {
                ReviewReportsAction::Create {
                    metadata: SubmissionMetadata {
                        venue: venue.trim().into(),
                        revision: revision.trim().into(),
                        submitted: submitted.trim().into(),
                        decision: decision.trim().into(),
                        deadline: deadline.trim().into(),
                    },
                }
            }
            (FormKind::Paste, [text]) => {
                let Some(selection) = selected else { return };
                ReviewReportsAction::Add {
                    submission: selection.submission.clone(),
                    source: ReviewReportSource::Paste { text: text.clone() },
                }
            }
            (FormKind::Point, [cards, response, locations]) => {
                let Some(Selection {
                    submission,
                    section: Section::Point(point),
                }) = selected
                else {
                    return;
                };
                ReviewReportsAction::Update {
                    submission: submission.clone(),
                    point: point.clone(),
                    update: ReviewPointUpdate {
                        cards: Some(card_ids(cards)),
                        response: Some(response.clone()),
                        fixed_locations: Some(
                            locations
                                .lines()
                                .map(str::trim)
                                .filter(|line| !line.is_empty())
                                .map(str::to_owned)
                                .collect(),
                        ),
                        ..Default::default()
                    },
                }
            }
            (FormKind::Draft(index), [range, summary, cards]) => {
                let lines = range
                    .split_once('-')
                    .map_or((range.as_str(), range.as_str()), |pair| pair);
                let (Ok(start), Ok(end)) = (
                    lines.0.trim().parse::<usize>(),
                    lines.1.trim().parse::<usize>(),
                ) else {
                    self.reviews.error = true;
                    cx.notify();
                    return;
                };
                let Some((id, number, points)) = &mut self.reviews.proposal else {
                    return;
                };
                let report = self
                    .reviews
                    .decks
                    .get(&self.deck)
                    .and_then(|deck| deck.submissions.iter().find(|entry| &entry.id == id))
                    .and_then(|entry| entry.reports.iter().find(|report| report.number == *number));
                if start == 0
                    || start > end
                    || summary.trim().is_empty()
                    || report.is_none_or(|r| end > r.text.lines().count())
                {
                    self.reviews.error = true;
                } else {
                    let draft = ReviewPointDraft {
                        lines: [start, end],
                        summary: summary.trim().into(),
                        cards: card_ids(cards),
                    };
                    let points = Arc::make_mut(points);
                    if let Some(index) = index {
                        points[index] = draft;
                    } else {
                        points.push(draft);
                    }
                    self.reviews.form = None;
                    self.reviews.error = false;
                }
                cx.notify();
                return;
            }
            _ => return,
        };
        self.save_review(action, cx);
    }

    fn choose_review_file(&mut self, submission: String, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("review-add-file").into()),
        });
        let deck = self.deck.clone();
        let epoch = self.connection_epoch;
        cx.spawn(async move |this, cx| {
            let result = match picker.await {
                Ok(Ok(Some(paths))) => {
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    cx.background_executor()
                        .spawn(async move {
                            let name = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            let mut bytes = Vec::new();
                            std::fs::File::open(path)?
                                .take(MAX_REVIEW_REPORT_FILE_BYTES + 1)
                                .read_to_end(&mut bytes)?;
                            anyhow::ensure!(
                                bytes.len() as u64 <= MAX_REVIEW_REPORT_FILE_BYTES,
                                "Report file too large"
                            );
                            Ok(ReviewReportSource::File {
                                name,
                                data: STANDARD.encode(bytes),
                            })
                        })
                        .await
                }
                Ok(Ok(None)) => return,
                _ => Err(anyhow::anyhow!("File picker failed")),
            };
            let _ = this.update(cx, |this, cx| {
                if this.connection_epoch != epoch || this.deck != deck || !this.reviews_visible() {
                    return;
                }
                match result {
                    Ok(source) => {
                        this.send_review(ReviewReportsAction::Add { submission, source }, cx)
                    }
                    Err(_) => {
                        this.reviews.error = true;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn review_markers(
        &self,
        card: &str,
        workbench: &Entity<Self>,
        cx: &gpui_kit::App,
    ) -> Option<gpui_kit::Div> {
        if !self.review_reports_available() {
            return None;
        }
        let deck = self.reviews.decks.get(&self.deck)?;
        let markers: Vec<_> = deck
            .submissions
            .iter()
            .flat_map(|entry| {
                entry
                    .points
                    .iter()
                    .filter(|point| point.cards.iter().any(|id| id == card))
                    .map(move |point| (entry, point))
            })
            .map(|(entry, point)| {
                let entity = workbench.clone();
                let deck = self.deck.clone();
                let target = (entry.id.clone(), point.report, Some(point.id.clone()));
                pill(
                    format!("review-marker-{card}-{}-{}", entry.id, point.id),
                    tr!("review-marker", point = point.id.clone()),
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.open_reviews(deck.clone(), Some(target.clone()), cx)
                    })
                })
            })
            .collect();
        (!markers.is_empty()).then(|| h_flex().gap_2().flex_wrap().children(markers))
    }

    pub(super) fn render_reviews(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let data = self.reviews.decks.get(&self.deck);
        let submission = self.reviews.selection.as_ref().and_then(|selection| {
            data?
                .submissions
                .iter()
                .find(|entry| entry.id == selection.submission)
        });
        let heading = v_flex()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("review-title")),
            )
            .children(self.reviews.error.then(|| {
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(tr!("review-error"))
            }));
        let mut content = v_flex().w_full().min_w_0().gap_4().p_4().child(heading);
        if let Some(form) = &self.reviews.form {
            content = content.child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .children(
                        form.fields
                            .iter()
                            .enumerate()
                            .map(|(index, field)| field.render(index)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                pill("review-form-save", tr!("review-save"), true, cx).on_click(
                                    cx.listener(|this, _, _, cx| this.submit_review_form(cx)),
                                ),
                            )
                            .child(
                                pill("review-form-cancel", tr!("review-cancel"), false, cx)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reviews.form = None;
                                        this.reviews.error = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        if let (Some(submission), Some(selection)) = (submission, &self.reviews.selection) {
            content = content.child(self.render_submission(
                data.unwrap(),
                submission,
                &selection.section,
                cx,
            ));
        } else {
            content = content
                .child(h_flex().child(
                    pill("review-create", tr!("review-create"), true, cx).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.review_form(FormKind::Create, None, window, cx);
                        },
                    )),
                ))
                .children(data.map(|data| {
                    v_flex()
                        .gap_2()
                        .children(data.submissions.iter().map(|entry| {
                            let id = entry.id.clone();
                            pill(
                                format!("review-submission-{id}"),
                                format!("{} · {}", entry.metadata.submitted, entry.metadata.venue),
                                false,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.select_review(id.clone(), Section::Points, cx)
                                },
                            ))
                        }))
                        .children(
                            data.submissions
                                .is_empty()
                                .then(|| div().text_sm().child(tr!("review-empty"))),
                        )
                }));
        }
        div()
            .id("review-pane")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .track_scroll(&self.reviews.scroll)
            .overflow_y_scrollbar()
            .child(content)
    }

    fn render_submission(
        &self,
        data: &DeckReviews,
        entry: &ReviewSubmission,
        section: &Section,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let id = entry.id.clone();
        let file_id = id.clone();
        let points_id = id.clone();
        let response_id = id.clone();
        let write_id = id.clone();
        let mut content = v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        pill("review-back", tr!("review-submissions"), false, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.reviews
                                    .scroll
                                    .set_offset(gpui_kit::point(px(0.), px(0.)));
                                this.reviews.selection = None;
                                this.reviews.form = None;
                                this.reviews.proposal = None;
                                this.reviews.error = false;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        pill(
                            "review-points",
                            tr!("review-points"),
                            matches!(section, Section::Points),
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_review(points_id.clone(), Section::Points, cx)
                        })),
                    )
                    .children(self.trash_button(
                        "review-delete-submission",
                        fuzukue_proto::TrashTarget::Submission { deck: self.deck.clone(), id: id.clone() },
                        id.clone(), cx,
                    ))
                    .children(entry.response.as_ref().map(|_| {
                        pill(
                            "review-response-view",
                            tr!("review-response-draft"),
                            matches!(section, Section::Response),
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_review(response_id.clone(), Section::Response, cx)
                        }))
                    })),
            )
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "{} · {}",
                entry.metadata.venue, entry.metadata.submitted
            )));
        let metadata = &entry.metadata;
        content = content.child(
            v_flex()
                .gap_1()
                .text_sm()
                .child(tr!(
                    "review-submitted-revision",
                    revision = metadata.revision.clone()
                ))
                .children((!metadata.decision.is_empty()).then(|| {
                    tr!(
                        "review-decision-value",
                        decision = metadata.decision.clone()
                    )
                }))
                .children((!metadata.deadline.is_empty()).then(|| {
                    tr!(
                        "review-deadline-value",
                        deadline = metadata.deadline.clone()
                    )
                })),
        );
        match section {
            Section::Points => {
                content = content
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                pill("review-add-paste", tr!("review-add-paste"), true, cx)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.review_form(FormKind::Paste, None, window, cx);
                                    })),
                            )
                            .child(
                                pill("review-add-file", tr!("review-add-file"), false, cx)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.choose_review_file(file_id.clone(), cx);
                                    })),
                            )
                            .children((!entry.points.is_empty()).then(|| {
                                pill(
                                    "review-write-response",
                                    if entry.response.is_some() {
                                        tr!("review-rewrite-response")
                                    } else {
                                        tr!("review-write-response")
                                    },
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.send_review(
                                            ReviewReportsAction::WriteResponse {
                                                submission: write_id.clone(),
                                            },
                                            cx,
                                        );
                                    },
                                ))
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .children(entry.reports.iter().map(|report| {
                                let id = id.clone();
                                let number = report.number;
                                let proposed = data.splits.iter().any(|split| {
                                    split.submission == entry.id
                                        && split.report == number
                                        && matches!(split.state, ReviewSplitState::Proposed { .. })
                                });
                                let label = if proposed {
                                    tr!("review-report-proposed", number = number.to_string())
                                } else {
                                    tr!("review-report", number = number.to_string())
                                };
                                pill(format!("review-report-{number}"), label, proposed, cx)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_review(id.clone(), Section::Report(number), cx)
                                    }))
                            })),
                    )
                    .child(self.render_point_table(entry, cx));
            }
            Section::Point(point) => {
                if let Some(point) = entry.points.iter().find(|p| &p.id == point) {
                    content = content.child(self.render_point(data, entry, point, cx));
                }
            }
            Section::Report(number) => {
                if let Some(report) = entry.reports.iter().find(|r| &r.number == number) {
                    content = content.child(self.render_report(data, entry, report, cx));
                }
            }
            Section::Response => {
                content = content.children(entry.response.as_ref().map(|text| {
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr!("review-response-draft")),
                        )
                        .child(review_quote(text.clone(), cx))
                }));
            }
        }
        content
    }

    fn render_point_table(&self, entry: &ReviewSubmission, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .child(
                h_flex().gap_1().flex_wrap().children(
                    std::iter::once(None)
                        .chain(STATUSES.map(Some))
                        .enumerate()
                        .map(|(i, filter)| {
                            pill(
                                format!("review-filter-{i}"),
                                filter.map_or(tr!("review-all"), status_label),
                                self.reviews.filter == filter,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.reviews.filter = filter;
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .children(
                entry
                    .points
                    .iter()
                    .filter(|point| {
                        self.reviews
                            .filter
                            .is_none_or(|status| point.status == status)
                    })
                    .map(|point| {
                        let submission = entry.id.clone();
                        let id = point.id.clone();
                        h_flex()
                            .id(format!("review-point-{id}"))
                            .gap_2()
                            .px_2()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|row| row.bg(theme.muted))
                            .child(div().flex_none().text_sm().child(id.clone()))
                            .child(div().flex_1().min_w_0().child(point.summary.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(status_label(point.status)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_review(
                                    submission.clone(),
                                    Section::Point(id.clone()),
                                    cx,
                                )
                            }))
                    }),
            )
    }

    fn render_point(
        &self,
        data: &DeckReviews,
        entry: &ReviewSubmission,
        point: &ReviewPoint,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let running = point
            .thread
            .as_ref()
            .is_some_and(|thread| data.fixing.contains(thread));
        let submission = entry.id.clone();
        let id = point.id.clone();
        let edit_point = point.clone();
        let can_fix = matches!(
            point.status,
            ReviewPointStatus::Todo | ReviewPointStatus::InProgress
        );
        v_flex()
            .id("review-point-detail")
            .gap_3()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} · {}", point.id, point.summary)),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().text_sm().child(tr!(
                                "review-quote-range",
                                start = point.lines[0].to_string(),
                                end = point.lines[1].to_string()
                            )))
                            .child(review_quote(point.quote.clone(), cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().text_sm().child(tr!("review-related-cards")))
                            .children(point.cards.iter().map(|id| {
                                let deck = self.deck.clone();
                                let id = id.clone();
                                let card = self.cards.iter().find(|card| card.id == id);
                                let label = card
                                    .filter(|card| !card.title.is_empty())
                                    .map_or(id.clone(), |card| card.title.clone());
                                let link = pill(format!("review-card-{id}"), label, false, cx)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.close_reviews();
                                        this.set_paper_pdf(false, cx);
                                        this.open_paper_from_chat(
                                            deck.clone(),
                                            Some(id.clone()),
                                            cx,
                                        );
                                    }));
                                v_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .child(link)
                                    .children(card.and_then(|card| {
                                        // Show the statement rendered like the paper view, not raw TeX.
                                        let range = fuzukue_view::card_section_ranges(&card.body)
                                            .into_iter()
                                            .find(|(heading, _)| super::paper::STATEMENT_HEADINGS.contains(&heading.as_ref()))
                                            .map(|(_, range)| range)?;
                                        let display = self.display_card_tex(&card.body[range]);
                                        (!display.markdown.trim().is_empty()).then(|| {
                                            div()
                                                .w_full()
                                                .min_w_0()
                                                .p_2()
                                                .rounded_md()
                                                .bg(cx.theme().muted)
                                                .text_sm()
                                                .child(super::paper::paper_text(
                                                    gpui_kit::component::text::TextView::markdown(
                                                        format!("review-card-body-{}", card.id),
                                                        display.markdown,
                                                    ),
                                                    cx,
                                                ))
                                        })
                                    }))
                            })),
                    ),
            )
            .children(
                (!point.response.is_empty()).then(|| review_quote(point.response.clone(), cx)),
            )
            .children(
                point
                    .fixed_locations
                    .iter()
                    .map(|location| div().text_sm().child(location.clone())),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(STATUSES.into_iter().enumerate().map(|(i, status)| {
                        let submission = entry.id.clone();
                        let point_id = point.id.clone();
                        pill(
                            format!("review-set-status-{i}"),
                            status_label(status),
                            point.status == status,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !running {
                                this.send_review(
                                    ReviewReportsAction::Update {
                                        submission: submission.clone(),
                                        point: point_id.clone(),
                                        update: ReviewPointUpdate {
                                            status: Some(status),
                                            ..Default::default()
                                        },
                                    },
                                    cx,
                                );
                            }
                        }))
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children((can_fix && !running).then(|| {
                        pill("review-fix", tr!("review-fix"), true, cx).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.send_review(
                                    ReviewReportsAction::Fix {
                                        submission: submission.clone(),
                                        point: id.clone(),
                                    },
                                    cx,
                                )
                            },
                        ))
                    }))
                    .children((!running).then(|| {
                        pill("review-edit-point", tr!("review-edit"), false, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.review_form(
                                    FormKind::Point,
                                    Some(edit_point.clone()),
                                    window,
                                    cx,
                                )
                            }),
                        )
                    }))
                    .children(running.then(|| div().text_sm().child(tr!("review-fixing"))))
                    .children(point.thread.as_ref().map(|thread| {
                        let thread = thread.clone();
                        pill("review-fix-thread", tr!("review-fix-thread"), false, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.select_thread(thread.clone(), cx)
                            }),
                        )
                    })),
            )
    }

    fn render_report(
        &self,
        data: &DeckReviews,
        entry: &ReviewSubmission,
        report: &ReviewReport,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let id = entry.id.clone();
        let number = report.number;
        let state = data
            .splits
            .iter()
            .find(|split| split.submission == id && split.report == number)
            .map(|split| &split.state);
        let confirmed = entry.split_reports.contains(&number)
            || entry.points.iter().any(|point| point.report == number);
        let label = tr!("review-report", number = number.to_string());
        let mut section = v_flex().id("review-report-detail").gap_2().child(
            h_flex().justify_between()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                .children(self.trash_button(
                    "review-delete-report",
                    fuzukue_proto::TrashTarget::Report { deck: self.deck.clone(), submission: id.clone(), number },
                    label, cx,
                )),
        );
        match (confirmed, state) {
            (true, _) => {}
            (false, Some(ReviewSplitState::Running)) => {
                section = section.child(tr!("review-splitting"));
            }
            (false, Some(ReviewSplitState::Proposed { points })) => {
                let points = self
                    .reviews
                    .proposal
                    .as_ref()
                    .filter(|(submission, report, _)| submission == &id && *report == number)
                    .map_or(points.as_slice(), |(_, _, points)| points.as_slice());
                section = section.child(self.render_review_proposal(&id, report, points, cx));
            }
            (false, state) => {
                let failed = matches!(state, Some(ReviewSplitState::Failed));
                section = section
                    .children(failed.then(|| div().text_sm().child(tr!("review-split-failed"))))
                    .child(h_flex().child(
                        pill("review-split", tr!("review-split"), true, cx).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.send_review(
                                    ReviewReportsAction::Split {
                                        submission: id.clone(),
                                        report: number,
                                    },
                                    cx,
                                )
                            },
                        )),
                    ));
            }
        }
        let text = report
            .text
            .lines()
            .enumerate()
            .map(|(i, line)| format!("{}  {line}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        section.child(review_quote(text, cx))
    }

    fn render_review_proposal(
        &self,
        id: &str,
        report: &ReviewReport,
        points: &[ReviewPointDraft],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let id = id.to_owned();
        let number = report.number;
        let drafts = Arc::new(points.to_vec());
        let add_id = id.clone();
        let add_points = drafts.clone();
        let confirm_id = id.clone();
        let lines = report.text.lines().collect::<Vec<_>>();
        v_flex()
            .id("review-proposal")
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("review-proposal")),
            )
            .children(points.iter().enumerate().map(|(index, point)| {
                let edit = (id.clone(), number, drafts.clone());
                let remove = edit.clone();
                let quote = lines
                    .get(point.lines[0].saturating_sub(1)..point.lines[1])
                    .unwrap_or_default()
                    .join("\n");
                v_flex()
                    .gap_2()
                    .child(div().child(point.summary.clone()))
                    .child(review_quote(quote, cx))
                    .children(
                        (!point.cards.is_empty())
                            .then(|| div().text_sm().child(point.cards.join(", "))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                pill(
                                    format!("review-draft-edit-{index}"),
                                    tr!("review-edit"),
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.reviews.proposal = Some(edit.clone());
                                        this.review_form(
                                            FormKind::Draft(Some(index)),
                                            None,
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            )
                            .child(
                                pill(
                                    format!("review-draft-remove-{index}"),
                                    tr!("review-remove"),
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let mut draft = remove.clone();
                                        Arc::make_mut(&mut draft.2).remove(index);
                                        this.reviews.form = None;
                                        this.reviews.proposal = Some(draft);
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        pill("review-draft-add", tr!("review-add-point"), false, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.reviews.proposal =
                                    Some((add_id.clone(), number, add_points.clone()));
                                this.review_form(FormKind::Draft(None), None, window, cx);
                            }),
                        ),
                    )
                    .child(
                        pill("review-confirm", tr!("review-confirm"), true, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if this.reviews.form.is_none() && this.reviews.saving.is_none() {
                                    this.save_review(
                                        ReviewReportsAction::Confirm {
                                            submission: confirm_id.clone(),
                                            report: number,
                                            points: drafts.as_ref().clone(),
                                        },
                                        cx,
                                    );
                                }
                            }),
                        ),
                    ),
            )
    }
}

fn review_area(
    label: &'static str,
    value: String,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Field {
    Field::Area(
        label,
        cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .default_value(value)
        }),
    )
}

fn review_quote(text: String, cx: &gpui_kit::App) -> gpui_kit::Div {
    div()
        .w_full()
        .min_w_0()
        .p_2()
        .rounded_md()
        .bg(cx.theme().muted)
        .text_sm()
        .child(text)
}
