//! **Text layer** (`markup` feature): pull a page's text and per-glyph rectangles
//! out of the PDF by running a custom [`hayro`] [`Device`] over it — no
//! rasterization, no heavyweight PDF library (only `kurbo` geometry, already in
//! hayro's tree).
//!
//! This is the substrate for text-anchored markup. A host can take a quote stored in
//! a note, [`PageText::locate`] it on the page, and draw a highlight over the
//! returned rectangles — all in **normalized** (0..1) page coordinates, so it's
//! independent of zoom and display DPI. Matching ignores whitespace, so a quote
//! survives the small spacing quirks of PDF text extraction.

use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, SoftMask, TransformExt, interpret_page,
};
use hayro::hayro_syntax::page::Rotation;
use kurbo::{Affine, BezPath, Point, Rect, Shape, Vec2};

use crate::Document;

/// A rectangle in normalized page coordinates: each component is a fraction (0..1) of
/// the page's width/height, origin at the top-left. Resolution- and zoom-independent,
/// so a host maps it to the on-screen page rect at paint time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NormRect {
    fn right(&self) -> f32 {
        self.x + self.w
    }
    fn bottom(&self) -> f32 {
        self.y + self.h
    }
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }
}

/// A point in normalized page coordinates (0..1 of width/height, top-left origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormPoint {
    pub x: f32,
    pub y: f32,
}

/// The result of a drag selection: the selected text (as a single-line quote), which
/// occurrence of that quote on the page it is (so it re-locates unambiguously), and
/// the rects to draw while selecting.
#[derive(Clone, Debug)]
pub struct Selection {
    pub quote: String,
    pub occurrence: usize,
    pub rects: Vec<NormRect>,
}

/// How a pointer gesture expands its endpoints.
#[derive(Clone, Copy, Default)]
pub(crate) enum SelectionKind {
    #[default]
    Drag,
    Word,
    Line,
}

/// A glyph cluster, with its visible ink and its baseline in unrotated page points.
struct Run {
    text: String,
    rect: NormRect,
    reading_x: f32,
    reading_right: f32,
    baseline: f32,
    em: f32,
}

struct Collector {
    runs: Vec<Run>,
    width: f32,
    height: f32,
    rotation: Rotation,
}

impl<'a> Device<'a> for Collector {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph_run(
        &mut self,
        run: &GlyphRun<'_, 'a>,
        props: DrawProps<'a>,
        // Invisible glyphs can be the OCR layer over a scanned page.
        _draw_mode: &DrawMode,
    ) {
        for glyph in run.glyphs() {
            self.glyph(glyph, props.transform * glyph.transform());
        }
    }
}

impl Collector {
    fn reading_point(&self, p: Point) -> Point {
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        match self.rotation {
            Rotation::None => p,
            Rotation::Horizontal => Point::new(p.y, w - p.x),
            Rotation::Flipped => Point::new(w - p.x, h - p.y),
            Rotation::FlippedHorizontal => Point::new(h - p.y, p.x),
        }
    }

    fn glyph(&mut self, glyph: &Glyph<'_>, transform: Affine) {
        let text = match glyph.as_unicode() {
            Some(BfString::Char(c)) => c.to_string(),
            Some(BfString::String(s)) => s,
            None => return,
        };
        let origin = transform.translation().to_point();
        let baseline = self.reading_point(origin);
        // Hayro places glyph outlines in 1000-unit font space.
        let em = (transform * Point::new(0.0, 1000.0) - origin).hypot() as f32;
        let (text, bbox) = if text.chars().all(char::is_whitespace) {
            // Spaces carry word boundaries even though they have no outline.
            (" ".to_owned(), Rect::from_origin_size(origin, (1.0, 1.0)))
        } else {
            let bbox = match glyph {
                Glyph::Outline(outline) => {
                    transform.transform_rect_bbox(outline.outline().bounding_box())
                }
                Glyph::Type3(_) => Rect::from_origin_size(origin - Vec2::new(0.0, 1.0), (1.0, 1.0)),
            };
            (text, bbox)
        };
        let x0 = (bbox.x0 as f32).clamp(0.0, self.width);
        let x1 = (bbox.x1 as f32).clamp(0.0, self.width);
        let y0 = (bbox.y0 as f32).clamp(0.0, self.height);
        let y1 = (bbox.y1 as f32).clamp(0.0, self.height);
        let a = self.reading_point(Point::new(x0.into(), y0.into()));
        let b = self.reading_point(Point::new(x1.into(), y1.into()));
        self.runs.push(Run {
            text,
            rect: NormRect {
                x: x0 / self.width,
                y: y0 / self.height,
                w: (x1 - x0) / self.width,
                h: (y1 - y0) / self.height,
            },
            reading_x: a.x.min(b.x) as f32,
            reading_right: a.x.max(b.x) as f32,
            baseline: baseline.y as f32,
            em,
        });
    }
}

/// Extract the text layer of page `index` (0-based) without rasterizing it.
pub fn extract_page_text(doc: &Document, index: usize) -> Option<PageText> {
    let page = doc.pages().get(index)?;
    let (width, height) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        page.initial_transform(true).to_kurbo(),
        Rect::new(0.0, 0.0, width.into(), height.into()),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut collector = Collector {
        runs: Vec::new(),
        width,
        height,
        rotation: page.rotation(),
    };
    interpret_page(page, &mut ctx, &mut collector);
    let reading_width = match page.rotation() {
        Rotation::Horizontal | Rotation::FlippedHorizontal => height,
        _ => width,
    };
    Some(PageText::with_dimensions(
        collector.runs,
        reading_width,
        width / height,
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Left,
    Right,
    Spanning,
}

struct Line {
    runs: Vec<usize>,
    column: Column,
}

struct Row {
    runs: Vec<usize>,
    baseline: f32,
    em: f32,
}

fn reading_lines(runs: &[Run], page_width: f32) -> Vec<Line> {
    let mut visible: Vec<_> = (0..runs.len())
        .filter(|&id| runs[id].rect.w > 0.0 && runs[id].rect.h > 0.0)
        .collect();
    visible.sort_by(|&a, &b| runs[a].baseline.total_cmp(&runs[b].baseline));

    // Group by baseline, allowing small baseline shifts for scripts in math.
    // Use the largest font as the reference so a tall union of ink cannot absorb
    // the next row and a tiny superscript does not become the row's baseline.
    let mut rows: Vec<Row> = Vec::new();
    for id in visible {
        let glyph = &runs[id];
        if let Some(row) = rows.last_mut() {
            if (row.baseline - glyph.baseline).abs() <= row.em.max(glyph.em) * 0.46 {
                row.runs.push(id);
                if glyph.em > row.em {
                    row.baseline = glyph.baseline;
                    row.em = glyph.em;
                }
                continue;
            }
        }
        rows.push(Row {
            runs: vec![id],
            baseline: glyph.baseline,
            em: glyph.em,
        });
    }

    // Repeated gaps only support columns if their right-hand fragments start
    // at a common margin. Justified text and inline math can have wide gaps
    // on several rows without lining up at that margin.
    let mut gaps = Vec::new();
    let mut previous_ink: Option<(usize, f32, f32, f32, f32)> = None;
    for (row_id, row) in rows.iter_mut().enumerate() {
        row.runs.sort_by(|&a, &b| {
            runs[a]
                .reading_x
                .total_cmp(&runs[b].reading_x)
                .then(a.cmp(&b))
        });
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        for &id in &row.runs {
            let glyph = &runs[id];
            if glyph.text == " " {
                continue;
            }
            if let Some((start, end)) = gutter_interval(right, glyph.reading_x, row.em, page_width) {
                gaps.push((row_id, start, end));
            }
            left = left.min(glyph.reading_x);
            right = right.max(glyph.reading_right);
        }
        // Columns with different leading can appear on adjacent baselines.
        if let Some((old_row, old_left, old_right, baseline, em)) = previous_ink
            && row.baseline - baseline <= 1.6 * row.em.max(em)
        {
            let (right_row, end, start, right_em) = if old_left < left {
                (row_id, old_right, left, row.em)
            } else {
                (old_row, right, old_left, em)
            };
            if let Some((start, end)) = gutter_interval(end, start, right_em, page_width) {
                gaps.push((right_row, start, end));
            }
        }
        if left < right {
            previous_ink = Some((row_id, left, right, row.baseline, row.em));
        }
    }
    let splits = column_gutter(&gaps, rows.len(), page_width).and_then(|(support, x)| {
        let splits: Vec<_> = rows.iter()
            .map(|row| split_at_gutter(row, runs, x, page_width))
            .collect();
        // A few spanning headings or equations are normal. Through-running
        // text as common as the aligned gaps makes the gutter ambiguous.
        (splits.iter().filter(|split| split.is_none()).count() < support).then_some(splits)
    });

    // Read down each column until a spanning row or a large vertical break,
    // then resume column-by-column below it.
    let mut lines = Vec::new();
    let mut right_column = Vec::new();
    let mut previous: Option<(f32, f32)> = None;
    for (row_id, mut row) in rows.into_iter().enumerate() {
        if previous.is_some_and(|(baseline, em)| row.baseline - baseline > 2.5 * em.max(row.em)) {
            lines.append(&mut right_column);
        }
        previous = Some((row.baseline, row.em));
        if let Some(split) = splits.as_ref().and_then(|splits| splits[row_id]) {
            let right = row.runs.split_off(split);
            if row.runs.iter().any(|&id| runs[id].text != " ") {
                lines.push(Line {
                    runs: row.runs,
                    column: Column::Left,
                });
            }
            if right.iter().any(|&id| runs[id].text != " ") {
                right_column.push(Line {
                    runs: right,
                    column: Column::Right,
                });
            }
            continue;
        }
        lines.append(&mut right_column);
        lines.push(Line {
            runs: row.runs,
            column: Column::Spanning,
        });
    }
    lines.append(&mut right_column);
    lines
}

fn column_gutter(
    gaps: &[(usize, f32, f32)],
    row_count: usize,
    page_width: f32,
) -> Option<(usize, f32)> {
    let mut edges: Vec<_> = gaps.iter()
        .flat_map(|&(row, start, end)| [(start, row, true), (end, row, false)])
        .collect();
    edges.sort_by(|(a, _, _), (b, _, _)| a.total_cmp(b));
    let mut active = vec![0; row_count];
    let mut support = 0;
    let mut best = None;
    for pair in edges.windows(2) {
        let [(start, row, opens), (end, _, _)] = [pair[0], pair[1]];
        // A staggered row can pair with both neighbors. Count it only once.
        if opens {
            support += usize::from(active[row] == 0);
            active[row] += 1;
        } else {
            active[row] -= 1;
            support -= usize::from(active[row] == 0);
        }
        if support < 3 || start >= end {
            continue;
        }
        let x = (start + end) * 0.5;
        let score = (support, -(x - page_width * 0.5).abs());
        if best.is_none_or(|(previous, _)| score > previous) {
            best = Some((score, x));
        }
    }
    best.map(|((support, _), x)| (support, x))
}

fn min_column_gap(em: f32, page_width: f32) -> f32 {
    (em * 2.5).max(page_width * 0.025)
}

fn gutter_interval(left_end: f32, right_start: f32, em: f32, width: f32) -> Option<(f32, f32)> {
    if right_start - left_end < min_column_gap(em, width)
        || left_end <= width * 0.16
        || right_start >= width * 0.84
    {
        return None;
    }
    let alignment = (em * 0.75).max(width * 0.01);
    Some((right_start - 2.0 * alignment, right_start - alignment))
}

fn split_at_gutter(row: &Row, runs: &[Run], x: f32, page_width: f32) -> Option<usize> {
    let split = row.runs.partition_point(|&id| runs[id].reading_x < x);
    let left_end = row.runs[..split]
        .iter()
        .filter(|&&id| runs[id].text != " ")
        .map(|&id| runs[id].reading_right)
        .reduce(f32::max);
    let right_start = row.runs[split..]
        .iter()
        .find(|&&id| runs[id].text != " ")
        .map(|&id| runs[id].reading_x);
    let crosses = match (left_end, right_start) {
        (Some(end), Some(start)) => start - end < min_column_gap(row.em, page_width),
        (Some(end), None) => end >= x,
        _ => false,
    };
    (!crosses).then_some(split)
}

#[derive(Clone)]
struct TextIndex {
    letters: String,
    // One entry per byte. Match offsets are always at UTF-8 boundaries.
    owner: Vec<usize>,
}

impl TextIndex {
    fn new(runs: &[Run], ids: impl IntoIterator<Item = usize>) -> Self {
        let mut letters = String::new();
        let mut owner = Vec::new();
        for id in ids {
            letters.extend(
                runs[id]
                    .text
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .flat_map(char::to_lowercase),
            );
            owner.resize(letters.len(), id);
        }
        Self { letters, owner }
    }

    fn matches<'a>(&'a self, key: &'a str) -> impl Iterator<Item = Vec<usize>> + 'a {
        let mut from = 0;
        std::iter::from_fn(move || {
            if key.is_empty() {
                return None;
            }
            let start = from + self.letters.get(from..)?.find(key)?;
            from = start + self.letters[start..].chars().next()?.len_utf8();
            let mut ids = self.owner[start..start + key.len()].to_vec();
            ids.dedup();
            Some(ids)
        })
    }
}

fn search_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Extracted glyphs and their visual reading order, with a separate index for
/// matching stored quotes in the original PDF stream order.
pub struct PageText {
    runs: Vec<Run>,
    lines: Vec<Line>,
    layout_order: Vec<(usize, usize)>,
    run_lines: Vec<Option<usize>>,
    stream_index: TextIndex,
    aspect: f32,
}

impl PageText {
    #[cfg(test)]
    fn new(runs: Vec<Run>) -> Self {
        Self::with_dimensions(runs, 1.0, 1.0)
    }

    fn with_dimensions(runs: Vec<Run>, width: f32, aspect: f32) -> Self {
        let lines = reading_lines(&runs, width);
        let mut layout_order = Vec::new();
        let mut run_lines = vec![None; runs.len()];
        for (line, row) in lines.iter().enumerate() {
            for &id in &row.runs {
                layout_order.push((line, id));
                run_lines[id] = Some(line);
            }
        }
        let stream_index = TextIndex::new(&runs, 0..runs.len());
        Self {
            runs,
            lines,
            layout_order,
            run_lines,
            stream_index,
            aspect,
        }
    }

    /// Whether the visible page has selectable text.
    pub fn is_empty(&self) -> bool {
        self.layout_order
            .iter()
            .all(|&(_, id)| self.runs[id].text == " ")
    }

    /// Reconstruct the page text in reading order.
    pub fn text(&self) -> String {
        self.join_runs(&self.layout_order, '\n')
    }

    fn join_runs(&self, order: &[(usize, usize)], line_break: char) -> String {
        let mut out = String::new();
        let mut previous: Option<(usize, &Run)> = None;
        for &(line, id) in order {
            let run = &self.runs[id];
            if let Some((prev_line, prev)) = previous {
                if prev_line != line {
                    out.truncate(out.trim_end().len());
                    out.push(line_break);
                } else if Self::word_gap(prev, run) {
                    out.push(' ');
                }
            }
            if run.text != " " || (!out.is_empty() && !out.ends_with(char::is_whitespace)) {
                out.push_str(&run.text);
            }
            previous = Some((line, run));
        }
        out.truncate(out.trim_end().len());
        out
    }

    fn word_gap(a: &Run, b: &Run) -> bool {
        a.text != " " && b.text != " " && b.reading_x - a.reading_right > 0.32 * a.em.max(b.em)
    }

    // Append reading-order matches after stream-order matches. Any occurrence
    // already stored by the viewer still addresses the same PDF glyphs.
    fn anchor_indices(&self) -> impl Iterator<Item = std::borrow::Cow<'_, TextIndex>>
    where
        TextIndex: Clone,
    {
        use std::borrow::Cow;
        std::iter::once(Cow::Borrowed(&self.stream_index)).chain(
            [None, Some(Column::Left), Some(Column::Right)]
                .into_iter()
                .map(|column| {
                    Cow::Owned(TextIndex::new(
                        &self.runs,
                        self.layout_order
                            .iter()
                            .filter(|&&(line, _)| self.in_column(line, column))
                            .map(|&(_, id)| id),
                    ))
                }),
        )
    }

    /// Locate a saved quote by its occurrence, preserving PDF stream-order anchors.
    pub fn locate(&self, needle: &str, occurrence: usize) -> Vec<NormRect> {
        self.find_anchor(&search_key(needle), |i, _| i == occurrence)
            .map_or_else(Vec::new, |(_, ids)| self.rects_for_runs(ids))
    }

    fn find_anchor(
        &self,
        key: &str,
        mut accepts: impl FnMut(usize, &[usize]) -> bool,
    ) -> Option<(usize, Vec<usize>)> {
        if key.is_empty() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        let mut occurrence = 0;
        for (stream, index) in self.anchor_indices().enumerate() {
            for ids in index.matches(key) {
                if stream != 0 && seen.contains(&ids) {
                    continue;
                }
                if accepts(occurrence, &ids) {
                    return Some((occurrence, ids));
                }
                seen.insert(ids);
                occurrence += 1;
            }
        }
        None
    }

    /// Every case- and whitespace-insensitive match in reading order.
    #[cfg(feature = "search")]
    pub fn find_matches(&self, needle: &str) -> Vec<Vec<NormRect>> {
        let key = search_key(needle);
        if key.is_empty() {
            return Vec::new();
        }
        let index = TextIndex::new(&self.runs, self.layout_order.iter().map(|&(_, id)| id));
        index
            .letters
            .match_indices(&key)
            .map(|(start, _)| {
                self.rects_for_runs(index.owner[start..start + key.len()].iter().copied())
            })
            .collect()
    }

    fn rects_for_runs(&self, ids: impl IntoIterator<Item = usize>) -> Vec<NormRect> {
        let mut by_line = std::collections::BTreeMap::<usize, NormRect>::new();
        for id in ids {
            let run = &self.runs[id];
            if let Some(line) = self.run_lines[id].filter(|_| run.text != " ") {
                by_line
                    .entry(line)
                    .and_modify(|r| *r = r.union(run.rect))
                    .or_insert(run.rect);
            }
        }
        by_line.into_values().collect()
    }

    fn distance(&self, p: NormPoint, r: NormRect) -> f32 {
        let dx = (r.x - p.x).max(p.x - r.right()).max(0.0) * self.aspect;
        let dy = (r.y - p.y).max(p.y - r.bottom()).max(0.0);
        dx * dx + dy * dy
    }

    fn nearest_run(&self, p: NormPoint) -> Option<usize> {
        self.layout_order
            .iter()
            .enumerate()
            .filter(|&(_, &(_, id))| self.runs[id].text != " ")
            .min_by(|&(_, &(_, a)), &(_, &(_, b))| {
                self.distance(p, self.runs[a].rect)
                    .total_cmp(&self.distance(p, self.runs[b].rect))
            })
            .map(|(i, _)| i)
    }

    fn in_column(&self, line: usize, column: Option<Column>) -> bool {
        column.is_none_or(|c| {
            matches!(self.lines[line].column, Column::Spanning) || self.lines[line].column == c
        })
    }

    fn selection_runs(
        &self,
        from: Option<NormPoint>,
        to: Option<NormPoint>,
        kind: SelectionKind,
    ) -> Vec<(usize, usize)> {
        let Some(last_index) = self.layout_order.len().checked_sub(1) else {
            return Vec::new();
        };
        let a = from.and_then(|p| self.nearest_run(p)).unwrap_or(0);
        let b = to.and_then(|p| self.nearest_run(p)).unwrap_or(last_index);
        let mut start = a.min(b);
        let mut end = a.max(b);
        let first = self.layout_order[start].0;
        let last = self.layout_order[end].0;
        let column =
            (from.is_some() && to.is_some() && self.lines[first].column == self.lines[last].column)
                .then_some(self.lines[first].column)
                .filter(|&column| column != Column::Spanning);
        while start > 0 && self.expands_across(start - 1, start, kind) {
            start -= 1;
        }
        while end + 1 < self.layout_order.len() && self.expands_across(end, end + 1, kind) {
            end += 1;
        }
        self.layout_order[start..=end]
            .iter()
            .copied()
            .filter(|&(line, _)| self.in_column(line, column))
            .collect()
    }

    fn expands_across(&self, a: usize, b: usize, kind: SelectionKind) -> bool {
        let (line_a, id_a) = self.layout_order[a];
        let (line_b, id_b) = self.layout_order[b];
        if line_a != line_b {
            return false;
        }
        let (a, b) = (&self.runs[id_a], &self.runs[id_b]);
        match kind {
            SelectionKind::Drag => false,
            SelectionKind::Line => true,
            SelectionKind::Word => word_cluster(a) && word_cluster(b) && !Self::word_gap(a, b),
        }
    }

    /// Text covered by the drag, preserving line breaks for the clipboard.
    pub fn selected_text(&self, from: NormPoint, to: NormPoint) -> Option<String> {
        self.selection_text(Some(from), Some(to), SelectionKind::Drag)
    }

    pub(crate) fn selection_text(
        &self,
        from: Option<NormPoint>,
        to: Option<NormPoint>,
        kind: SelectionKind,
    ) -> Option<String> {
        let text = self.join_runs(&self.selection_runs(from, to, kind), '\n');
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn selection_rects(
        &self,
        from: Option<NormPoint>,
        to: Option<NormPoint>,
        kind: SelectionKind,
    ) -> Vec<NormRect> {
        self.rects_for_runs(
            self.selection_runs(from, to, kind)
                .into_iter()
                .map(|(_, id)| id),
        )
    }

    /// Resolve a drag to its one-line quote, saved occurrence and highlight rectangles.
    pub fn select(&self, from: NormPoint, to: NormPoint) -> Option<Selection> {
        let order = self.selection_runs(Some(from), Some(to), SelectionKind::Drag);
        let quote = self.join_runs(&order, ' ');
        if quote.is_empty() {
            return None;
        }
        let key = search_key(&quote);
        let ids: Vec<_> = order
            .into_iter()
            .map(|(_, id)| id)
            .filter(|&id| self.runs[id].text != " ")
            .collect();
        let (occurrence, _) = self.find_anchor(&key, |_, matched| matched == ids)?;
        Some(Selection {
            quote,
            occurrence,
            rects: self.rects_for_runs(ids),
        })
    }
}

fn word_cluster(run: &Run) -> bool {
    run.text.chars().all(|c| {
        // One click cluster for scripts normally written without word spaces.
        c.is_alphanumeric() && !matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, x: f32, y: f32, w: f32, h: f32) -> Run {
        Run {
            text: text.to_string(),
            rect: NormRect { x, y, w, h },
            reading_x: x,
            reading_right: x + w,
            baseline: y + h,
            em: h * 2.0,
        }
    }

    // "Hello World" on one line, then "second" on the next.
    fn sample() -> PageText {
        PageText::new(vec![
            run("Hello", 0.10, 0.10, 0.10, 0.02),
            run("World", 0.22, 0.10, 0.10, 0.02),
            run("second", 0.10, 0.14, 0.12, 0.02),
        ])
    }

    #[test]
    fn locate_ignores_case_and_whitespace() {
        let pt = sample();
        // "hello world" with a space matches "Hello"+"World" (no space stored).
        let rects = pt.locate("hello world", 0);
        assert_eq!(rects.len(), 1); // one line
        let r = rects[0];
        // Spans from Hello's x to World's right edge.
        assert!((r.x - 0.10).abs() < 1e-4);
        assert!((r.right() - 0.32).abs() < 1e-4);
    }

    #[test]
    fn selected_text_keeps_line_breaks() {
        let pt = sample();
        let from = NormPoint { x: 0.11, y: 0.11 };
        let to = NormPoint { x: 0.2, y: 0.15 };
        assert_eq!(
            pt.selected_text(from, to).as_deref(),
            Some("Hello World\nsecond")
        );
    }

    #[test]
    fn locate_missing_is_empty() {
        assert!(sample().locate("absent", 0).is_empty());
    }

    #[test]
    fn locate_occurrence_index() {
        let pt = PageText::new(vec![
            run("cat", 0.1, 0.1, 0.06, 0.02),
            run("cat", 0.1, 0.2, 0.06, 0.02),
        ]);
        assert_eq!(pt.locate("cat", 0)[0].y, 0.1);
        assert_eq!(pt.locate("cat", 1)[0].y, 0.2);
        assert!(pt.locate("cat", 2).is_empty());
    }

    #[test]
    fn multi_line_match_yields_a_rect_per_line() {
        // "Helloworldsecond" spans two lines (Hello+World on line 1, second on line 2).
        let rects = sample().locate("HelloWorldsecond", 0);
        assert_eq!(rects.len(), 2);
        assert!(rects[0].y < rects[1].y);
    }

    #[test]
    fn text_reconstruction_inserts_space_and_newline() {
        let t = sample().text();
        assert_eq!(t, "Hello World\nsecond");
    }

    #[test]
    fn letter_spacing_keeps_words_whole_and_recorded_space_breaks_them() {
        // Tracked "Counters for": the letters have small intra-word gaps that must NOT
        // split (the old 0.25*line_h gap heuristic mis-read them as spaces — "Co u nte
        // rs"), and the PDF's own space glyph (text " ", sitting low at the baseline so
        // its center_y differs from the letters') provides the single word break —
        // without forcing a line break (it must be excluded from the baseline compare).
        let mut runs = Vec::new();
        for (i, c) in "Counters".chars().enumerate() {
            runs.push(run(
                &c.to_string(),
                0.10 + i as f32 * 0.025,
                0.10,
                0.02,
                0.02,
            ));
        }
        runs.push(run(" ", 0.30, 0.118, 0.002, 0.002));
        for (i, c) in "for".chars().enumerate() {
            runs.push(run(
                &c.to_string(),
                0.31 + i as f32 * 0.025,
                0.10,
                0.02,
                0.02,
            ));
        }
        let pt = PageText::new(runs);
        assert_eq!(pt.text(), "Counters for");
        // And a drag across both words yields the same clean, single-spaced quote.
        let sel = pt
            .select(
                NormPoint { x: 0.105, y: 0.11 },
                NormPoint { x: 0.370, y: 0.11 },
            )
            .unwrap();
        assert_eq!(sel.quote, "Counters for");
    }

    #[test]
    fn select_spans_runs_into_a_quote() {
        let pt = sample();
        // Drag from inside "Hello" to inside "World".
        let sel = pt
            .select(
                NormPoint { x: 0.12, y: 0.11 },
                NormPoint { x: 0.27, y: 0.11 },
            )
            .unwrap();
        assert_eq!(sel.quote, "Hello World");
        assert_eq!(sel.occurrence, 0);
        assert_eq!(sel.rects.len(), 1);
    }

    #[test]
    fn select_occurrence_counts_earlier_matches() {
        let pt = PageText::new(vec![
            run("cat", 0.1, 0.1, 0.06, 0.02),
            run("cat", 0.1, 0.2, 0.06, 0.02),
        ]);
        // Selecting the second "cat" reports occurrence 1.
        let sel = pt
            .select(NormPoint { x: 0.12, y: 0.2 }, NormPoint { x: 0.14, y: 0.2 })
            .unwrap();
        assert_eq!(sel.quote, "cat");
        assert_eq!(sel.occurrence, 1);
    }

    #[cfg(feature = "search")]
    #[test]
    fn find_matches_returns_all_occurrences_in_reading_order() {
        let pt = PageText::new(vec![
            run("cat", 0.1, 0.1, 0.06, 0.02),
            run("dog", 0.1, 0.2, 0.06, 0.02),
            run("cat", 0.1, 0.3, 0.06, 0.02),
        ]);
        let ms = pt.find_matches("CAT"); // case-insensitive
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0][0].y, 0.1); // first occurrence, top
        assert_eq!(ms[1][0].y, 0.3); // second, lower
        assert!(pt.find_matches("zebra").is_empty());
    }
}
