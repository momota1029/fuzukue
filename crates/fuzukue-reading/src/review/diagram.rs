use std::{collections::HashMap, fmt::Write as _};

use super::model::{Finding, Graph, Tier};

const BOX_WIDTH: usize = 112;
const BOX_HEIGHT: usize = 34;
const COLUMN_GAP: usize = 52;
const ROW_GAP: usize = 14;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub(super) fn render(graph: &Graph<'_>, findings_by_step: &HashMap<&str, Vec<&Finding>>) -> String {
    let mut depths = HashMap::new();
    let mut columns: Vec<(usize, Vec<&str>)> = Vec::new();
    let mut column_indices = HashMap::new();

    // The reference SVG orders columns and rows by first encounter in
    // dependency-first traversal. Keep that order even for ungrouped steps.
    let mut place = |id, depth| {
        let index = *column_indices.entry(depth).or_insert_with(|| {
            columns.push((depth, Vec::new()));
            columns.len() - 1
        });
        columns[index].1.push(id);
    };
    for step in graph.steps.iter().filter(|step| step.id != graph.target) {
        let mut visit = vec![(step.id.as_str(), false)];
        while let Some((id, visited_dependencies)) = visit.pop() {
            if depths.contains_key(id) {
                continue;
            }
            let dependencies = graph.by_id[id]
                .deps
                .iter()
                .map(String::as_str)
                .filter(|&dep| dep != graph.target);
            if visited_dependencies {
                let depth = dependencies.map(|dep| depths[dep] + 1).max().unwrap_or(0);
                depths.insert(id, depth);
                place(id, depth);
            } else {
                visit.push((id, true));
                visit.extend(dependencies.rev().map(|dep| (dep, false)));
            }
        }
    }
    let target_depth = depths.values().copied().max().unwrap_or(0) + 1;
    place(graph.target, target_depth);

    let mut positions = HashMap::new();
    let mut ordered_nodes = Vec::new();
    let mut rows = 0;
    for (depth, ids) in columns {
        rows = rows.max(ids.len());
        for (row, id) in ids.into_iter().enumerate() {
            let x = 20 + depth * (BOX_WIDTH + COLUMN_GAP);
            let y = 16 + row * (BOX_HEIGHT + ROW_GAP);
            positions.insert(id, (x, y));
            ordered_nodes.push((id, x, y));
        }
    }
    let width = 40 + (target_depth + 1) * (BOX_WIDTH + COLUMN_GAP);
    let height = 30 + rows * (BOX_HEIGHT + ROW_GAP);
    let mut svg = format!(
        r##"<svg width="100%" viewBox="0 0 {width} {height}" xmlns="http://www.w3.org/2000/svg" role="img"><defs><marker id="ar" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="#9aa4b5"/></marker></defs>"##
    );
    for step in graph.steps {
        let (x2, y2) = positions[step.id.as_str()];
        for dependency in &step.deps {
            let (x1, y1) = positions[dependency.as_str()];
            write!(
                svg,
                r##"<path d="M{},{y1} C{},{y1} {},{y2} {x2},{y2}" fill="none" stroke="#c3cad6" stroke-width="1.2" marker-end="url(#ar)"/>"##,
                x1 + BOX_WIDTH,
                x1 + BOX_WIDTH + COLUMN_GAP / 2,
                x2 as isize - (COLUMN_GAP / 2) as isize,
                y1 = y1 + BOX_HEIGHT / 2,
                y2 = y2 + BOX_HEIGHT / 2,
            )
            .expect("writing to a string");
        }
    }
    for (id, x, y) in ordered_nodes {
        let step = graph.by_id[id];
        let escaped_id = escape(id);
        let title = escape(&step.title.chars().take(11).collect::<String>());
        let (anchor, fill) = if id == graph.target {
            ("intro".into(), "#1d2430")
        } else {
            let fill = match step.tier {
                Tier::Key => "#c2410c",
                Tier::Semi => "#6b4fbb",
                Tier::Standard => "#5b6677",
            };
            (format!("step-{escaped_id}"), fill)
        };
        let center = x + BOX_WIDTH / 2;
        write!(
            svg,
            r##"<a href="#{anchor}"><rect x="{x}" y="{y}" width="{BOX_WIDTH}" height="{BOX_HEIGHT}" rx="7" fill="{fill}"/><text x="{center}" y="{}" fill="#fff" font-size="12.5" font-weight="700" text-anchor="middle">{escaped_id}</text><text x="{center}" y="{}" fill="#fff" font-size="10" text-anchor="middle" opacity=".9">{title}</text></a>"##,
            y + 14,
            y + 27,
        )
        .expect("writing to a string");
        if let Some(findings) = findings_by_step.get(id) {
            let count = findings.len();
            let cx = x + BOX_WIDTH - 4;
            write!(
                svg,
                r##"<circle cx="{cx}" cy="{}" r="8" fill="#fff" stroke="#a32020"/><text x="{cx}" y="{}.5" font-size="10" text-anchor="middle" fill="#a32020" font-weight="700">{count}</text>"##,
                y + 2,
                y + 5,
            )
            .expect("writing to a string");
        }
    }
    svg.push_str("</svg>");
    svg
}
