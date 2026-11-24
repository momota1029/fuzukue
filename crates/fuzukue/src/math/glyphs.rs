use std::{borrow::Cow, collections::HashMap, rc::Rc};

use gpui_kit::{
    canvas, fill, font, point, px, size, App, Bounds, FillOptions, FillRule, Font, FontStyle,
    FontWeight, Hsla, IntoElement, Path, PathBuilder, PathStyle, Pixels, Point, Rgba, ShapedLine,
    Size, Styled, TextAlign, TextRun, Window,
};
use ratex_layout::{layout, to_display_list, validate_layout, LayoutOptions};
use ratex_types::{
    color::Color, display_item::DisplayItem, math_style::MathStyle, path_command::PathCommand,
};

pub(super) fn register_fonts(cx: &App) {
    let fonts = [
        "AMS-Regular",
        "Caligraphic-Bold",
        "Caligraphic-Regular",
        "Fraktur-Bold",
        "Fraktur-Regular",
        "Main-Bold",
        "Main-BoldItalic",
        "Main-Italic",
        "Main-Regular",
        "Math-BoldItalic",
        "Math-Italic",
        "SansSerif-Bold",
        "SansSerif-Italic",
        "SansSerif-Regular",
        "Script-Regular",
        "Size1-Regular",
        "Size2-Regular",
        "Size3-Regular",
        "Size4-Regular",
        "Typewriter-Regular",
    ];
    let data = fonts
        .iter()
        .map(|name| {
            ratex_katex_fonts::ttf_bytes(&format!("KaTeX_{name}.ttf")).expect("bundled KaTeX font")
        })
        .collect();
    if let Err(error) = cx.text_system().add_fonts(data) {
        eprintln!("KaTeX フォントを登録できない: {error:#}");
    }
}

fn glyph_font(name: &str) -> Font {
    let (family, variant) = name.split_once('-').unwrap_or(("Main", "Regular"));
    if matches!(family, "CJK" | "Emoji") {
        return font("KaTeX_Main");
    }
    let mut font = font(format!("KaTeX_{family}"));
    if variant.starts_with("Bold") {
        font.weight = FontWeight::BOLD;
    }
    if variant.ends_with("Italic") {
        font.style = FontStyle::Italic;
    }
    font
}

fn hsla(color: Color) -> Hsla {
    Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    }
    .into()
}

enum Draw {
    Glyph(GlyphPaint),
    Quad(Bounds<Pixels>, Hsla),
    Rule {
        bounds: Bounds<Pixels>,
        color: Hsla,
        dashed: bool,
    },
    Path(Path<Pixels>, Hsla),
}

enum GlyphPaint {
    Text {
        line: ShapedLine,
        top: Point<Pixels>,
    },
    Outline(Path<Pixels>, Hsla),
}

#[derive(Default)]
pub(super) struct GlyphFonts(HashMap<String, GlyphFont>);

struct GlyphFont {
    font: Font,
    id: gpui_kit::FontId,
    primary_loaded: bool,
    outline_face: Option<ttf_parser::Face<'static>>,
}

impl GlyphFont {
    fn new(name: &str, window: &Window) -> Self {
        let font = glyph_font(name);
        let text = window.text_system();
        let id = text.resolve_font(&font);
        // Some GPUI backends reject sparse fonts as primary text faces.
        // Their outlines still give us the exact delimiter or symbol.
        let primary_loaded = text
            .get_font_for_id(id)
            .is_some_and(|resolved| resolved.family == font.family);
        let outline_face = match ratex_katex_fonts::ttf_bytes(&format!("KaTeX_{name}.ttf")) {
            Some(Cow::Borrowed(bytes)) => ttf_parser::Face::parse(bytes, 0).ok(),
            _ => None,
        };
        Self {
            font,
            id,
            primary_loaded,
            outline_face,
        }
    }

    fn paint_at(
        &self,
        character: char,
        baseline: Point<Pixels>,
        size: Pixels,
        color: Hsla,
        window: &Window,
    ) -> GlyphPaint {
        if !self.primary_loaded {
            if let Some(path) = self.outline(character, baseline, size) {
                return GlyphPaint::Outline(path, color);
            }
        }
        let text = character.to_string();
        let line = window.text_system().shape_line(
            text.clone().into(),
            size,
            &[TextRun {
                len: text.len(),
                font: self.font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        if self.primary_loaded && line.runs.iter().any(|run| run.font_id != self.id) {
            // Keep the text system's pixel hinting for very thin outlines,
            // such as a minus sign in a superscript.
            if let Some(path) =
                self.outline(character, baseline, size).filter(|path| {
                    path.bounds.size.height >= px(1.) && path.bounds.size.width >= px(1.)
                })
            {
                return GlyphPaint::Outline(path, color);
            }
        }
        let top = baseline - point(px(0.), line.ascent);
        GlyphPaint::Text { line, top }
    }

    fn outline(
        &self,
        character: char,
        baseline: Point<Pixels>,
        size: Pixels,
    ) -> Option<Path<Pixels>> {
        let face = self.outline_face.as_ref()?;
        let glyph = face.glyph_index(character)?;
        let unit = size / f32::from(face.units_per_em());
        let mut outline = GlyphOutline {
            path: PathBuilder::fill().with_style(PathStyle::Fill(
                FillOptions::default().with_fill_rule(FillRule::NonZero),
            )),
            baseline,
            unit,
        };
        face.outline_glyph(glyph, &mut outline)?;
        outline.path.build().ok()
    }
}

/// Adapts an embedded TrueType outline (y up) to a GPUI path (y down).
struct GlyphOutline {
    path: PathBuilder,
    baseline: Point<Pixels>,
    unit: Pixels,
}

impl GlyphOutline {
    fn point(&self, x: f32, y: f32) -> Point<Pixels> {
        self.baseline + point(self.unit * x, -self.unit * y)
    }
}

impl ttf_parser::OutlineBuilder for GlyphOutline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to(self.point(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(self.point(x, y));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.path.curve_to(self.point(x, y), self.point(x1, y1));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.path
            .cubic_bezier_to(self.point(x, y), self.point(x1, y1), self.point(x2, y2));
    }
    fn close(&mut self) {
        self.path.close();
    }
}

pub(super) struct Formula {
    pub size: Size<Pixels>,
    pub baseline: Pixels,
    draw: Vec<Draw>,
}

impl Formula {
    pub fn element(self: Rc<Self>) -> impl IntoElement {
        let size = self.size;
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| self.paint(bounds.origin, window, cx),
        )
        .w(size.width)
        .h(size.height)
        .flex_shrink_0()
    }


    pub fn new(
        nodes: &[ratex_parser::ParseNode],
        font_size: f32,
        rgba: Rgba,
        display: bool,
        window: &Window,
        fonts: &mut GlyphFonts,
    ) -> Option<Self> {
        let options = LayoutOptions {
            style: if display {
                MathStyle::Display
            } else {
                MathStyle::Text
            },
            color: Color::new(rgba.r, rgba.g, rgba.b, rgba.a),
            ..Default::default()
        };
        let laid_out = layout(nodes, &options);
        validate_layout(&laid_out).ok()?;
        let list = to_display_list(&laid_out);
        let em = |value: f64| px(value as f32 * font_size);
        // Leave room for italic overhang and subpixel antialiasing.
        let padding = px((font_size / 16.0).max(1.0));
        let position = |x, y| point(padding + em(x), padding + em(y));
        let mut formula = Self {
            size: size(
                em(list.width).max(px(0.)) + 2. * padding,
                em(list.total_height()) + 2. * padding,
            ),
            baseline: padding + em(list.height),
            draw: Vec::with_capacity(list.items.len()),
        };
        for item in list.items {
            match item {
                DisplayItem::GlyphPath {
                    x,
                    y,
                    scale,
                    font,
                    char_code,
                    color,
                } => {
                    let character = char::from_u32(char_code)?;
                    let glyph_size = em(scale);
                    let baseline = position(x, y);
                    let glyph_font = fonts
                        .0
                        .entry(font.clone())
                        .or_insert_with(|| GlyphFont::new(&font, window));
                    let paint = glyph_font.paint_at(character, baseline, glyph_size, hsla(color), window);
                    formula.draw.push(Draw::Glyph(paint));
                }
                DisplayItem::Line {
                    x,
                    y,
                    width,
                    thickness,
                    color,
                    dashed,
                } => {
                    let height = em(thickness);
                    let bounds = Bounds::new(
                        position(x, y) - point(px(0.), height / 2.),
                        size(em(width), height),
                    );
                    formula.draw.push(Draw::Rule {
                        bounds,
                        color: hsla(color),
                        dashed,
                    });
                }
                DisplayItem::Rect {
                    x,
                    y,
                    width,
                    height,
                    color,
                } => {
                    formula.draw.push(Draw::Quad(
                        Bounds::new(position(x, y), size(em(width), em(height))),
                        hsla(color),
                    ));
                }
                DisplayItem::Path {
                    x,
                    y,
                    commands,
                    fill,
                    color,
                } => {
                    let origin = position(x, y);
                    // As in RaTeX's renderer, filled subpaths are composited independently.
                    let mut starts = vec![0];
                    if fill {
                        starts.extend(commands.iter().enumerate().skip(1).filter_map(
                            |(index, cmd)| {
                                matches!(cmd, PathCommand::MoveTo { .. }).then_some(index)
                            },
                        ));
                    }
                    starts.push(commands.len());
                    for range in starts.windows(2) {
                        let path = build_path(&commands[range[0]..range[1]], fill, origin, em)?;
                        formula.draw.push(Draw::Path(path, hsla(color)));
                    }
                }
            }
        }
        Some(formula)
    }

    fn paint(&self, origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
        for draw in &self.draw {
            match draw {
                Draw::Glyph(paint) => {
                    match paint {
                        GlyphPaint::Text { line, top } => {
                            let _ = line.paint(
                                origin + *top,
                                line.ascent + line.descent,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        GlyphPaint::Outline(path, color) => {
                            paint_path(path, origin, *color, window)
                        }
                    }
                }
                Draw::Quad(bounds, color) => {
                    window.paint_quad(fill(
                        Bounds::new(origin + bounds.origin, bounds.size),
                        *color,
                    ));
                }
                Draw::Rule {
                    bounds,
                    color,
                    dashed,
                } => {
                    let mut bounds = *bounds;
                    // Keep thin rules visible on the active display without
                    // baking device pixel ratio into the cached layout.
                    let height = bounds.size.height.max(px(1. / window.scale_factor()));
                    bounds.origin += origin;
                    bounds.origin.y -= (height - bounds.size.height) / 2.;
                    bounds.size.height = height;
                    if *dashed {
                        paint_dashes(bounds, *color, window);
                    } else {
                        window.paint_quad(fill(bounds, *color));
                    }
                }
                Draw::Path(path, color) => paint_path(path, origin, *color, window),
            }
        }
    }
}

fn paint_path(path: &Path<Pixels>, origin: Point<Pixels>, color: Hsla, window: &mut Window) {
    let mut path = path.clone();
    path.bounds.origin += origin;
    for vertex in &mut path.vertices {
        vertex.xy_position += origin;
    }
    window.paint_path(path, color);
}

fn build_path(
    commands: &[PathCommand],
    fill: bool,
    origin: Point<Pixels>,
    em: impl Fn(f64) -> Pixels,
) -> Option<Path<Pixels>> {
    let mut path = if fill {
        PathBuilder::fill()
    } else {
        PathBuilder::stroke(px(1.5))
    };
    let p = |x, y| origin + point(em(x), em(y));
    for command in commands {
        match *command {
            PathCommand::MoveTo { x, y } => path.move_to(p(x, y)),
            PathCommand::LineTo { x, y } => path.line_to(p(x, y)),
            PathCommand::QuadTo { x1, y1, x, y } => path.curve_to(p(x, y), p(x1, y1)),
            PathCommand::CubicTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                path.cubic_bezier_to(p(x, y), p(x1, y1), p(x2, y2));
            }
            PathCommand::Close => path.close(),
        }
    }
    path.build().ok()
}

fn paint_dashes(bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let left = bounds.left();
    let right = bounds.right();
    let y = bounds.top();
    let dash = bounds.size.height * 4.;
    let period = 2. * dash;
    let clip = window.content_mask().bounds;
    // Begin at the first dash that can intersect the visible area.
    let first = (f32::from((clip.left() - left) / period)).floor().max(0.);
    let count = (f32::from((clip.right().min(right) - left) / period)).ceil() - first;
    for index in 0..count as usize {
        let x = left + period * (first + index as f32);
        window.paint_quad(fill(
            Bounds::new(point(x, y), size(dash.min(right - x), bounds.size.height)),
            color,
        ));
    }
}
