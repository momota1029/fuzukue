//! Merge the tool and approval summaries that nothing visible separates.
//!
//! Hidden rows and turn boundaries keep a run going. A visible message, card or
//! unanswered approval ends it. Rows stay as they are. The first row of a run
//! shows one summary of all its calls and decided approvals.

use super::{Chat, Content, Row, ToolGroup};
use fuzukue_proto::ApprovalState;
use std::collections::HashMap;

type Position = (usize, usize);

#[derive(Default)]
pub(super) struct ToolRun {
    pub rows: Vec<usize>,
    pub calls: Vec<Position>,
    /// Decided approval requests, in transcript order.
    pub decided: Vec<Position>,
    pub attached: HashMap<Position, usize>,
    pub unlinked: usize,
}

impl ToolRun {
    pub fn root(&self) -> usize {
        self.rows[0]
    }

    pub fn approvals(&self) -> usize {
        self.unlinked + self.attached.values().sum::<usize>()
    }

    pub fn open(&self, rows: &[Row]) -> bool {
        self.rows.iter().any(|&row| matches!(&rows[row].content, Content::Tools(group) if group.open))
    }

    pub fn decided_open(&self, rows: &[Row]) -> bool {
        self.rows.iter().any(|&row| matches!(&rows[row].content, Content::Tools(group) if group.decided_open))
    }
}

/// An unanswered request keeps its own line where it was asked.
fn mergeable(group: &ToolGroup) -> bool {
    (!group.calls.is_empty() || !group.approvals.is_empty())
        && group.approvals.iter().all(|approval| approval.resolved.is_some())
}

pub(super) struct ToolRuns {
    run_by_row: Vec<Option<usize>>,
    runs: Vec<ToolRun>,
    pub standalone: HashMap<usize, usize>,
    pub denials: HashMap<usize, Vec<String>>,
}

impl ToolRuns {
    /// `visible` says whether the transcript shows a row at all.
    pub fn new(chat: &Chat, visible: impl Fn(&Row) -> bool) -> Self {
        let rows = &chat.rows;
        let mut layout = Self {
            run_by_row: vec![None; rows.len()], runs: Vec::new(),
            standalone: HashMap::new(), denials: HashMap::new(),
        };
        let mut current = None::<usize>;
        for (row_index, row) in rows.iter().enumerate() {
            let group = match &row.content {
                Content::Tools(group) if mergeable(group) => group,
                _ if !visible(row) => continue,
                _ => {
                    current = None;
                    continue;
                }
            };
            let run_index = *current.get_or_insert_with(|| {
                layout.runs.push(ToolRun::default());
                layout.runs.len() - 1
            });
            layout.run_by_row[row_index] = Some(run_index);
            let run = &mut layout.runs[run_index];
            run.rows.push(row_index);
            run.calls.extend((0..group.calls.len()).map(|call| (row_index, call)));
            run.decided.extend((0..group.approvals.len()).map(|approval| (row_index, approval)));
        }

        for receipt in chat.approval_receipts.values().filter(|receipt| receipt.job().is_none()) {
            let Some(row) = receipt.history_anchor().and_then(|item| chat.row_for_approval_anchor(item)) else { continue };
            let run = layout.run_by_row[row];
            if receipt.state == ApprovalState::Denied {
                let row = run.map_or(row, |run| layout.runs[run].root());
                layout.denials.entry(row).or_default().push(receipt.tool.clone());
            } else if receipt.automatic {
                if let Some(run) = run.map(|index| &mut layout.runs[index]) {
                    if let Some(call) = receipt.tool_item.as_ref().and_then(|item| chat.by_tool.get(item)) {
                        *run.attached.entry(*call).or_default() += 1;
                    } else {
                        run.unlinked += 1;
                    }
                } else {
                    *layout.standalone.entry(row).or_default() += 1;
                }
            }
        }
        for denials in layout.denials.values_mut() {
            denials.sort_unstable();
        }
        layout
    }

    pub fn at(&self, row: usize) -> Option<&ToolRun> {
        self.run_by_row[row].map(|run| &self.runs[run])
    }

    pub fn hides(&self, row: usize) -> bool {
        self.at(row).is_some_and(|run| run.root() != row)
    }
}
