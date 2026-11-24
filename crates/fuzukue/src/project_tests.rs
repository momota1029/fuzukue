use std::collections::{HashMap, HashSet};

use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, ActiveTheme as _, Sizable as _,
};
use gpui_kit::{div, prelude::*, Context};
use fuzukue_proto::{ClientMsg, TestRun, TestState, TestSuite};

use crate::{icons::Symbol, RightPane, Workbench};

#[derive(Default)]
pub(super) struct TestsPane {
    suites: Vec<TestSuite>,
    runs: HashMap<String, TestRun>,
    expanded: HashSet<String>,
    listing: bool,
    error: Option<String>,
}

impl TestsPane {
    pub(super) fn disconnect(&mut self) {
        self.listing = false;
        self.runs.clear();
    }
}

impl Workbench {
    pub(super) fn show_tests(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.right = RightPane::Tests;
        self.refresh_test_suites(cx);
    }

    pub(super) fn refresh_test_suites(&mut self, cx: &mut Context<Self>) {
        self.tests.listing = self.out.send(ClientMsg::ListTests).is_ok();
        cx.notify();
    }

    pub(super) fn update_test_suites(&mut self, suites: Vec<TestSuite>, error: Option<String>) {
        self.tests.listing = false;
        self.tests.suites = suites;
        self.tests.error = error;
    }

    pub(super) fn update_test_run(&mut self, run: TestRun) {
        if self
            .tests
            .runs
            .get(&run.suite)
            .is_none_or(|previous| previous.revision < run.revision)
        {
            self.tests.runs.insert(run.suite.clone(), run);
        }
    }

    pub(super) fn render_tests_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let panel = v_flex()
            .id("tests-pane")
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                h_flex()
                    .flex_none()
                    .px(crate::pane_resize::PANE_INSET)
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().flex_1().child(tr!("tests-tests")))
                    .child(
                        Button::new("tests-refresh")
                            .ghost()
                            .xsmall()
                            .label(tr!("tests-rediscover"))
                            .disabled(self.tests.listing)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_test_suites(cx))),
                    )
                    .child(
                        Button::new("tests-close")
                            .ghost()
                            .xsmall()
                            .icon(Symbol::X.icon())
                            .tooltip(tr!("tests-close-tests"))
                            .accessibility_label(tr!("tests-close-tests"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_right_pane(cx))),
                    ),
            );
        let empty = if self.tests.listing {
            Some(tr!("tests-discovering-tests"))
        } else if self.tests.suites.is_empty() && self.tests.error.is_none() {
            Some(tr!("tests-no-tests-found"))
        } else {
            None
        };
        panel.child(
            div()
                .id("tests-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(
                    v_flex()
                        .w_full()
                        .p_3()
                        .gap_3()
                        .text_sm()
                        .children(
                            empty.map(|text| div().text_color(theme.muted_foreground).child(text)),
                        )
                        .children(
                            self.tests
                                .error
                                .as_ref()
                                .map(|error| div().child(error.clone())),
                        )
                        .children(
                            self.tests
                                .suites
                                .iter()
                                .map(|suite| self.render_test_suite(suite, cx)),
                        ),
                ),
        )
    }

    fn render_test_suite(&self, suite: &TestSuite, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let id = &suite.id;
        let run = self.tests.runs.get(id);
        let running = run.is_some_and(|run| run.state == TestState::Running);
        let expanded = self.tests.expanded.contains(id);
        let row = v_flex()
            .id(format!("tests-suite-{id}"))
            .w_full()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(div().min_w_0().truncate().child(id.clone()))
                    .child(
                        Button::new(format!("tests-run-{id}"))
                            .small()
                            .label(if running { tr!("tests-stop") } else { tr!("tests-run") })
                            .disabled(!running && suite.unavailable.is_some())
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, _| {
                                    let request = if running {
                                        ClientMsg::StopTests { suite: id.clone() }
                                    } else {
                                        ClientMsg::RunTests {
                                            suite: id.clone(),
                                            filter: None,
                                        }
                                    };
                                    let _ = this.out.send(request);
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} · {}", suite.cwd, suite.command)),
            )
            .children(suite.unavailable.as_ref().map(|program| {
                div()
                    .text_xs()
                    .child(tr!("tests-unavailable-not-found", program = program.to_string()))
            }));
        let Some(run) = run else { return row };
        row.child(
            div()
                .id(format!("tests-result-{id}"))
                .text_xs()
                .child(tr!("tests-run-result", state = state_label(run.state).to_string(), passed = run.passed.to_string(), failed = run.failed.to_string())),
        )
        .children(run.failures.iter().enumerate().map(|(index, failure)| {
            let message = failure.message.clone();
            if let Some(path) = &failure.file {
                let label = failure
                    .line
                    .map_or_else(|| path.clone(), |line| format!("{path}:{line}"));
                let path = path.clone();
                let lines = failure.line.map(|line| line as usize..=line as usize);
                Button::new(format!("tests-failure-{id}-{index}"))
                    .ghost()
                    .small()
                    .label(format!("{label} · {message}"))
                    .max_w_full()
                    .justify_start()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_source_file(path.clone(), lines.clone(), window, cx);
                    }))
                    .into_any_element()
            } else {
                div().text_xs().child(message).into_any_element()
            }
        }))
        .when(!run.output.is_empty(), |row| {
            row.child(
                Button::new(format!("tests-output-{id}"))
                    .ghost()
                    .xsmall()
                    .label(if expanded {
                        tr!("tests-hide-output")
                    } else {
                        tr!("tests-show-output")
                    })
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            if !this.tests.expanded.remove(&id) {
                                this.tests.expanded.insert(id.clone());
                            }
                            cx.notify();
                        }
                    })),
            )
            .when(expanded, |row| {
                row.child(
                    div()
                        .w_full()
                        .p_2()
                        .bg(theme.secondary)
                        .text_xs()
                        .child(run.output.clone()),
                )
            })
        })
    }
}

fn state_label(state: TestState) -> &'static str {
    match state {
        TestState::Running => tr!("tests-running"),
        TestState::Passed => tr!("tests-passed"),
        TestState::Failed => tr!("tests-failed"),
        TestState::Cancelled => tr!("tests-stop"),
        TestState::TimedOut => tr!("tests-timed-out"),
        TestState::Unavailable => tr!("tests-unavailable"),
    }
}
