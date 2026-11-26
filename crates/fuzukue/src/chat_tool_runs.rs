use super::{Actor, Chat, Content, Row, RowKind};
use fuzukue_proto::ApprovalState;
use std::collections::HashMap;

type Position = (usize, usize);

#[derive(Default)]
pub(super) struct ToolRun {
    pub rows: Vec<usize>,
    pub calls: Vec<Position>,
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
}

pub(super) struct ToolRuns {
    run_by_row: Vec<Option<usize>>,
    runs: Vec<ToolRun>,
    pub standalone: HashMap<usize, usize>,
    pub denials: HashMap<usize, Vec<String>>,
}

impl ToolRuns {
    pub fn new(chat: &Chat) -> Self {
        let rows = &chat.rows;
        let mut layout = Self {
            run_by_row: vec![None; rows.len()], runs: Vec::new(),
            standalone: HashMap::new(), denials: HashMap::new(),
        };
        let mut current = None::<usize>;
        let mut actor = None::<&Actor>;
        for (row_index, row) in rows.iter().enumerate() {
            let group = match &row.content {
                Content::Tools(group) if group.approvals.is_empty() && !group.calls.is_empty() => group,
                _ if row.kind(false) == RowKind::Hidden => continue,
                _ => {
                    current = None;
                    actor = None;
                    continue;
                }
            };
            if actor.is_some_and(|previous| previous != &row.actor) {
                current = None;
            }
            actor = Some(&row.actor);
            let run_index = *current.get_or_insert_with(|| {
                layout.runs.push(ToolRun::default());
                layout.runs.len() - 1
            });
            layout.run_by_row[row_index] = Some(run_index);
            let run = &mut layout.runs[run_index];
            run.rows.push(row_index);
            run.calls.extend((0..group.calls.len()).map(|call| (row_index, call)));
            if row.ends_tool_run {
                current = None;
                actor = None;
            }
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
