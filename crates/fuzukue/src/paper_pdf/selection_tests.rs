use gpui_pdf::{extract_page_text, page_dims, parse, render_page, NormPoint};

fn pdf(content: &str, boxes: &str) -> Vec<u8> {
    pdf_with_mapping(content, boxes, None)
}

fn pdf_with_mapping(content: &str, boxes: &str, unicode: Option<&str>) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!("<< /Type /Page /Parent 2 0 R {boxes} /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    if let Some(unicode) = unicode {
        let font = &mut objects[4];
        font.insert_str(font.len() - 2, "/ToUnicode 6 0 R ");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{unicode}\nendstream",
            unicode.len()
        ));
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

// An untagged paper whose PDF stream emits the right column first on each row.
const PAPER: &str = "
BT /F1 19 Tf 105 752 Td (A Synthetic Two-Column Paper) Tj ET
BT /F1 11 Tf 317 709 Td (Right column first paragraph) Tj ET
BT /F1 11 Tf 52 711 Td (Left column first paragraph) Tj ET
BT /F1 11 Tf 317 693 Td (Right column second paragraph) Tj ET
BT /F1 11 Tf 52 695 Td (Left column second paragraph) Tj ET
BT /F1 13 Tf 285 650 Td (E = mc) Tj /F1 8 Tf 5 Ts (2) Tj -3 Ts (i) Tj 0 Ts ET
0.8 g 65 616 460 14 re f 0 g
BT /F1 11 Tf 317 605 Td (Right column third paragraph) Tj ET
BT /F1 11 Tf 52 607 Td (Left column third paragraph) Tj ET
BT /F1 11 Tf 317 589 Td (Right column fourth paragraph) Tj ET
BT /F1 11 Tf 52 591 Td (Left column fourth paragraph) Tj ET
";

const LETTER: &str = "/MediaBox [0 0 612 792]";

fn at(x: f32, y: f32) -> NormPoint {
    NormPoint {
        x: x / 612.,
        y: (792. - y) / 792.,
    }
}

#[test]
fn drag_in_one_column_keeps_copy_and_highlight_in_that_column() {
    let doc = parse(pdf(PAPER, LETTER).into()).unwrap();
    let text = extract_page_text(&doc, 0).unwrap();
    let start = at(53., 715.);
    let end = at(208., 697.);
    let expected = "Left column first paragraph\nLeft column second paragraph";
    assert_eq!(text.selected_text(start, end).as_deref(), Some(expected));
    assert_eq!(text.selected_text(end, start).as_deref(), Some(expected));

    let selected = text.select(start, end).unwrap();
    assert_eq!(selected.rects.len(), 2);
    assert!(selected.rects.iter().all(|r| r.x + r.w < 0.5));
    assert_eq!(
        text.locate(&selected.quote, selected.occurrence),
        selected.rects
    );

    // Existing anchors use stream order, including an old quote crossing columns.
    assert_eq!(
        text.locate(
            "Right column first paragraph Left column first paragraph",
            0
        )
        .len(),
        2
    );
}

#[test]
fn full_width_elements_separate_column_groups() {
    let doc = parse(pdf(PAPER, LETTER).into()).unwrap();
    let text = extract_page_text(&doc, 0).unwrap();
    assert_eq!(
        text.text(),
        "A Synthetic Two-Column Paper\nLeft column first paragraph\nLeft column second paragraph\nRight column first paragraph\nRight column second paragraph\nE = mc2i\nLeft column third paragraph\nLeft column fourth paragraph\nRight column third paragraph\nRight column fourth paragraph"
    );
    let from = at(53., 714.);
    let to = at(208., 594.);
    let selection = text.select(from, to).unwrap();
    assert_eq!(
        text.selected_text(from, to).as_deref(),
        Some("Left column first paragraph\nLeft column second paragraph\nE = mc2i\nLeft column third paragraph\nLeft column fourth paragraph")
    );
    assert_eq!(
        text.locate(&selection.quote, selection.occurrence),
        selection.rects
    );
    assert_eq!(selection.rects.len(), 5);
}

#[test]
fn unicode_clusters_and_hyphenation_keep_the_pdf_text() {
    let mapping = "
/CIDInit /ProcSet findresource begin
12 dict begin begincmap
/CMapType 2 def
1 begincodespacerange
<00> <FF>
endcodespacerange
3 beginbfchar
<41> <65E5>
<42> <672C>
<43> <006600660069>
endbfchar
endcmap CMapName currentdict /CMap defineresource pop end end";
    let content = "BT /F1 14 Tf 40 705 Td (AB oCce multi-) Tj
        0 -18 Td (line AB) Tj ET";
    let doc = parse(pdf_with_mapping(content, LETTER, Some(mapping)).into()).unwrap();
    let text = extract_page_text(&doc, 0).unwrap();
    assert_eq!(text.text(), "日本 office multi-\nline 日本");
    assert_eq!(text.locate("日本", 1).len(), 1);
    assert!(text.locate("日本", 0)[0].y < text.locate("日本", 1)[0].y);
    let ligature = text.locate("ffi", 0)[0];
    let pos = NormPoint {
        x: ligature.x + ligature.w * 0.5,
        y: ligature.y + ligature.h * 0.5,
    };
    assert_eq!(text.selected_text(pos, pos).as_deref(), Some("ffi"));
}

#[test]
fn crop_clips_selection_to_the_same_edge_as_the_raster() {
    for rotation in [0, 90, 180, 270] {
        let specification = format!(
            "/MediaBox [-10 -20 310 410] /CropBox [42.25 40.5 270.75 340.75] /Rotate {rotation}"
        );
        let doc = parse(pdf("BT /F1 30 Tf 33 152 Td (Hg) Tj ET", &specification).into()).unwrap();
        let [rect] = extract_page_text(&doc, 0).unwrap().locate("Hg", 0)[..] else {
            panic!("expected one highlight for Hg on a {rotation}-degree page");
        };
        let (pw, ph) = page_dims(&doc)[0];
        for scale in [0.75, 1., 1.25, 1.5, 2., 3.25] {
            let image = render_page(&doc, 0, scale).unwrap();
            let size = image.size(0);
            let w = size.width.0 as usize;
            let h = size.height.0 as usize;
            let mut ink = [w, h, 0, 0];
            for (i, pixel) in image.as_bytes(0).unwrap().chunks_exact(4).enumerate() {
                if pixel[0] < 128 {
                    ink[0] = ink[0].min(i % w);
                    ink[1] = ink[1].min(i / w);
                    ink[2] = ink[2].max(i % w + 1);
                    ink[3] = ink[3].max(i / w + 1);
                }
            }
            assert!(ink[0] < ink[2] && ink[1] < ink[3]);
            let highlight = [
                rect.x * w as f32,
                rect.y * h as f32,
                (rect.x + rect.w) * w as f32,
                (rect.y + rect.h) * h as f32,
            ];
            for (ink, edge) in ink.into_iter().zip(highlight) {
                assert!((ink as f32 - edge).abs() < 2.,
                    "rotation {rotation}, page {pw} x {ph}, scale {scale}: ink {ink}, highlight {edge}, {rect:?}");
            }
        }
    }
}
