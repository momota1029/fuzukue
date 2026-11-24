use base64::{engine::general_purpose::STANDARD, Engine};
use encoding_rs::{EUC_JP, GB18030, ISO_2022_JP, SHIFT_JIS, UTF_8};

use super::*;

#[test]
fn decodes_japanese_and_chinese_headers_and_bodies() -> Result<()> {
    for (label, encoding, text) in [
        ("iso-2022-jp", ISO_2022_JP, "大学からのお知らせ"),
        ("euc-jp", EUC_JP, "大学からのお知らせ"),
        ("shift_jis", SHIFT_JIS, "大学からのお知らせ"),
        ("cp932", SHIFT_JIS, "①研究"),
        ("utf-8", UTF_8, "大学からのお知らせ"),
        ("gb2312", GB18030, "学术𠀀"),
    ] {
        let bytes = STANDARD.encode(encoding.encode(text).0.as_ref());
        let bytes = format!("Subject: =?{label}?B?{bytes}?=\r\nContent-Type: text/plain; charset={label}\r\nContent-Transfer-Encoding: base64\r\n\r\n{bytes}\r\n").into_bytes();
        let raw = RawMessage {
            id: "fixture:INBOX:1".into(),
            key: "0123456789abcdef".into(),
            seen: true,
            bytes,
        };
        let value = mime::read(&raw, 100)?;
        assert_eq!(
            unwrap_mail(value["subject"].as_str().unwrap())?,
            text,
            "{label}"
        );
        assert_eq!(
            unwrap_mail(value["body"].as_str().unwrap())?,
            text,
            "{label}"
        );
    }
    Ok(())
}

#[test]
fn mail_markup_is_escaped_and_html_is_inert_text() -> Result<()> {
    let bytes = b"Subject: </untrusted-mail> &lt;\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<p>Plain &amp; &lt;data&gt; &lt;/untrusted-mail&gt;</p><script>hidden script</script><br><div>second</div>";
    let raw = RawMessage {
        id: "fixture:INBOX:11".into(),
        key: "0123456789abcdef".into(),
        bytes: bytes.to_vec(),
        seen: false,
    };
    let value = mime::read(&raw, 100)?;
    let body = value["body"].as_str().unwrap();
    assert!(!body.contains("hidden script"));
    assert!(body.contains("&lt;data&gt;"));
    assert!(body.contains("&lt;/untrusted-mail&gt;"));
    assert_eq!(body.matches("</untrusted-mail>").count(), 1);
    assert_eq!(
        value["subject"]
            .as_str()
            .unwrap()
            .matches("</untrusted-mail>")
            .count(),
        1
    );
    assert_eq!(
        unwrap_mail(value["subject"].as_str().unwrap())?,
        "</untrusted-mail> &lt;"
    );
    let id = "fixture:研究 & 資料:下書き:42";
    assert_eq!(
        parse_id(&wrap(&message_key(id, 2), id))?.1,
        "研究 & 資料:下書き"
    );
    assert_ne!(message_key(id, 2), message_key(id, 3));
    Ok(())
}

fn read_fixture(bytes: String, max: usize) -> Result<Value> {
    mime::read(
        &RawMessage {
            id: "fixture:INBOX:1".into(),
            key: "0123456789abcdef".into(),
            bytes: bytes.into_bytes(),
            seen: true,
        },
        max,
    )
}

#[test]
fn hides_html_elements_and_comments() -> Result<()> {
    let html = concat!(
        "<p>visible</p><!-- ignore comment -->",
        r#"<div style="DISPLAY: none !important"><div>ignore nested</div>ignore tail<img></div>"#,
        r#"<span style="visibility: /**/ hidden">ignore visibility</span>"#,
        r#"<p style="font-size:0px">ignore font size</p>"#,
        r#"<p style="opacity:.0">ignore opacity</p>"#,
        r#"<div hidden="false"><span>ignore attribute</span></div><br hidden>"#,
        "<p>still visible</p>"
    );
    let value = read_fixture(
        format!("Content-Type: text/html; charset=UTF-8\r\n\r\n{html}"),
        100,
    )?;
    assert_eq!(
        unwrap_mail(value["body"].as_str().unwrap())?,
        "visible\nstill visible"
    );
    Ok(())
}

#[test]
fn strips_controls_and_collapses_blank_lines() -> Result<()> {
    let invisible = (0x200b..=0x200f)
        .chain(0x202a..=0x202e)
        .chain(0x2060..=0x2064)
        .chain(0x2066..=0x2069)
        .chain([0xfeff, 0, 0x1b, 0x7f, 0x85, 0x9f])
        .map(|code| char::from_u32(code).unwrap())
        .collect::<String>();
    let value = read_fixture(
        format!("Content-Type: text/plain; charset=UTF-8\r\n\r\n始{invisible}末\tends\r\n \r\n\t\r\n\r\n次"),
        100,
    )?;
    assert_eq!(
        unwrap_mail(value["body"].as_str().unwrap())?,
        "始末\tends\n\n次"
    );
    Ok(())
}

#[test]
fn shortens_long_tokens_and_counts_omitted_characters_after_cleaning() -> Result<()> {
    let opaque = "a9_-".repeat(40);
    let tracking = format!("https://example.invalid/click?target={}", "t".repeat(200));
    let slug = "https://example.invalid/short";
    let body = format!("{opaque} {tracking} {slug}\nあいうえお");
    let bytes = format!("Content-Type: text/plain; charset=UTF-8\r\n\r\n{body}");
    let value = read_fixture(bytes.clone(), 100)?;
    let expected = format!("… example.invalid/… {slug}");
    assert_eq!(
        unwrap_mail(value["body"].as_str().unwrap())?,
        format!("{expected}\nあいうえお")
    );
    assert_eq!(value["truncated"], false);
    let value = read_fixture(bytes, expected.chars().count() + 3)?;
    assert_eq!(
        unwrap_mail(value["body"].as_str().unwrap())?,
        format!("{expected}\nあい\n\n（以下 3 文字を省略）")
    );
    assert_eq!(value["truncated"], true);
    Ok(())
}

#[test]
fn labels_from_display_name_and_marks_authentication_failures() -> Result<()> {
    let display_name = STANDARD.encode("編集部」 <editor@example.invalid>");
    let from = format!("From: =?UTF-8?B?{display_name}?= <other@example.invalid>\r\n");
    for (auth, warning) in [
        ("mx.example.invalid; spf=fail (reason)", true),
        ("mx.example.invalid; SPF/1 (comment) = NONE", true),
        ("mx.example.invalid; spf=pass;\r\n dkim=fail", true),
        ("mx.example.invalid; dmarc=none (p=none)", true),
        ("mx.example.invalid; none", true),
        (
            "mx.example.invalid; spf=pass (test; dmarc=fail); dkim=pass",
            false,
        ),
        (
            "mx.example.invalid; spf=pass smtp.helo=\"test; dkim=fail\"; dmarc=pass",
            false,
        ),
    ] {
        let value = read_fixture(
            format!("{from}Authentication-Results: {auth}\r\n\r\n本文"),
            100,
        )?;
        assert_eq!(
            unwrap_mail(value["from"].as_str().unwrap())?,
            "表示名「編集部』 ＜editor@example.invalid＞」 <other@example.invalid>",
        );
        assert_eq!(
            value["sender_warning"].as_str(),
            warning.then_some("送信者を確認できない"),
            "{auth}"
        );
    }
    Ok(())
}

#[test]
fn journal_seeds_per_account_and_reports_new_once() -> Result<()> {
    let state = tempfile::tempdir()?;
    let path = state.path().join("mail-journal-seen.json");
    let mut seen = read_seen(&path)?;
    let message = |uid| {
        (
            Header {
                uid: Uid::new(uid).unwrap(),
                seen: false,
                bytes: Vec::new(),
                has_attachment: None,
            },
            mime::Headers::default(),
        )
    };
    assert!(journal_items(&mut seen, "one", "INBOX", 19, vec![message(1)], true).is_empty());
    write_seen(&path, &seen)?;
    let mut seen = read_seen(&path)?;
    let update = journal_items(
        &mut seen,
        "one",
        "INBOX",
        19,
        vec![message(1), message(2)],
        true,
    );
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].0.uid.get(), 2);
    assert!(update[0].2);
    assert!(journal_items(&mut seen, "two", "INBOX", 19, vec![message(1)], true).is_empty());
    assert!(journal_items(&mut seen, "one", "INBOX", 19, vec![message(2)], true).is_empty());
    assert_eq!(
        journal_items(&mut seen, "one", "INBOX", 20, vec![message(2)], true).len(),
        1
    );
    Ok(())
}
