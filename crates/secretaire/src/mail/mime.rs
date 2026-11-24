use std::{borrow::Cow, sync::LazyLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use encoding_rs::{Encoding, GB18030, ISO_2022_JP, SHIFT_JIS, UTF_8};
use html5gum::{DefaultEmitter, StartTag, Token, Tokenizer};
use mailparse::{DispositionType, MailAddr, MailAddrList, MailHeader, ParsedMail};
use regex::bytes::{Captures, Regex};
use serde_json::{json, Value};
use url::Url;

use super::{wrap, RawMessage};

#[derive(Default)]
pub(super) struct Headers {
    from: Option<MailAddrList>,
    auth_failed: bool,
    to: String,
    cc: String,
    date: String,
    pub(super) subject: String,
    pub(super) message_id: String,
    pub(super) references: String,
    pub(super) in_reply_to: String,
}

impl Headers {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        let (headers, _) = mailparse::parse_headers(bytes)
            .map_err(|_| anyhow::anyhow!("メールヘッダーを解析できない"))?;
        Ok(Self::from_headers(&headers))
    }

    fn from_headers(headers: &[MailHeader<'_>]) -> Self {
        let mut fields = Self::default();
        for header in headers.iter().rev() {
            let field = match header.get_key_ref().to_ascii_lowercase().as_str() {
                "from" => {
                    fields.from = parse_from(header);
                    continue;
                }
                "authentication-results" => {
                    fields.auth_failed |= authentication_failed(&header.get_value());
                    continue;
                }
                "to" => &mut fields.to,
                "cc" => &mut fields.cc,
                "date" => &mut fields.date,
                "subject" => &mut fields.subject,
                "message-id" => &mut fields.message_id,
                "references" => &mut fields.references,
                "in-reply-to" => &mut fields.in_reply_to,
                _ => continue,
            };
            *field = decode_header(header).replace(['\r', '\n'], " ");
        }
        fields.subject = strip_controls(&fields.subject);
        fields
    }

    pub(super) fn display_from(&self) -> String {
        let Some(addresses) = &self.from else {
            return "差出人アドレス不明".into();
        };
        let senders = addresses.iter().flat_map(|address| match address {
            MailAddr::Single(single) => std::slice::from_ref(single),
            MailAddr::Group(group) => &group.addrs,
        });
        let display = senders
            .map(|sender| {
                let address = clean_header(&sender.addr).replace(['<', '>'], "�");
                match sender.display_name.as_deref() {
                    Some(name) => {
                        let name = clean_header(name)
                            .replace('「', "『")
                            .replace('」', "』")
                            .replace('<', "＜")
                            .replace('>', "＞");
                        format!("表示名「{name}」 <{address}>")
                    }
                    None => format!("<{address}>"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if display.is_empty() {
            "差出人アドレス不明".into()
        } else {
            display
        }
    }

    pub(super) fn sender_address(&self) -> Option<&str> {
        match self.from.as_deref()?.as_slice() {
            [MailAddr::Single(sender)] => Some(&sender.addr),
            _ => None,
        }
    }

    pub(super) fn sender_warning(&self) -> Option<&'static str> {
        self.auth_failed.then_some("送信者を確認できない")
    }

    pub(super) fn when(&self) -> Option<DateTime<Utc>> {
        if self.date.trim().is_empty() {
            return None;
        }
        DateTime::parse_from_rfc2822(&self.date)
            .or_else(|_| DateTime::parse_from_rfc3339(&self.date))
            .map(|date| date.to_utc())
            .ok()
            .or_else(|| {
                mailparse::dateparse(&self.date)
                    .ok()
                    .and_then(|n| DateTime::from_timestamp(n, 0))
            })
    }

    pub(super) fn display_date(&self) -> String {
        self.when()
            .map(|date| date.to_rfc3339())
            .unwrap_or_else(|| self.date.clone())
    }
}

fn normalized_encoded_words(raw: &[u8]) -> Cow<'_, [u8]> {
    // Mailers use CP932 for Windows Shift_JIS and sometimes label GB18030 as GB2312/GBK.
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)=\?(cp932|gb2312|gbk|x-gbk)\?").unwrap());
    LABEL.replace_all(raw, |label: &Captures<'_>| {
        if label[1].eq_ignore_ascii_case(b"cp932") {
            &b"=?Shift_JIS?"[..]
        } else {
            &b"=?GB18030?"[..]
        }
    })
}

fn parse_from(header: &MailHeader<'_>) -> Option<MailAddrList> {
    let raw = header.get_value_raw();
    if let Cow::Owned(value) = normalized_encoded_words(raw) {
        let bytes = [b"From: ".as_slice(), &value].concat();
        let (rewritten, _) = mailparse::parse_header(&bytes).ok()?;
        return mailparse::addrparse_header(&rewritten).ok();
    }
    if has_iso_2022_escape(raw) {
        return mailparse::addrparse(&ISO_2022_JP.decode(raw).0).ok();
    }
    mailparse::addrparse_header(header).ok()
}

fn has_iso_2022_escape(raw: &[u8]) -> bool {
    raw.windows(3)
        .any(|window| matches!(window, b"\x1b\x24B" | b"\x1b\x24@"))
}

fn decode_header(header: &MailHeader<'_>) -> String {
    let raw = header.get_value_raw();
    if let Cow::Owned(value) = normalized_encoded_words(raw) {
        let bytes = [b"X: ".as_slice(), &value].concat();
        if let Ok((rewritten, _)) = mailparse::parse_header(&bytes) {
            return rewritten.get_value();
        }
    }
    if has_iso_2022_escape(raw) {
        return ISO_2022_JP.decode(raw).0.into_owned();
    }
    header.get_value()
}

fn authentication_failed(value: &str) -> bool {
    static FAILED: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i);\s*(?:(?:spf|dkim|dmarc)(?:/\d+)?\s*=\s*(?:fail|none)(?:\s|;|$)|none\s*$)",
        )
        .unwrap()
    });
    // Ignore comments and quoted property values, where semicolons aren't separators.
    let mut clauses = String::new();
    let (mut comment_depth, mut quoted, mut escaped) = (0, false, false);
    for ch in value.chars() {
        if escaped {
            escaped = false;
        } else if comment_depth > 0 {
            match ch {
                '(' => comment_depth += 1,
                ')' => comment_depth -= 1,
                '\\' => escaped = true,
                _ => {}
            }
        } else if quoted {
            match ch {
                '"' => quoted = false,
                '\\' => escaped = true,
                _ => {}
            }
        } else {
            match ch {
                '(' => {
                    comment_depth = 1;
                    clauses.push(' ');
                }
                '"' => {
                    quoted = true;
                    clauses.push('"');
                }
                _ => clauses.push(ch),
            }
        }
    }
    FAILED.is_match(&clauses)
}

fn filename(part: &ParsedMail<'_>) -> Option<String> {
    let disposition = part.get_content_disposition();
    let filename = disposition
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"));
    if let Some(name) = filename.filter(|s| !s.trim().is_empty()) {
        // RFC 2231 parameters are already decoded by mailparse. Handle encoded words too.
        let header = format!("X: {name}");
        return Some(
            mailparse::parse_header(header.as_bytes())
                .ok()
                .map_or_else(|| name.clone(), |(header, _)| decode_header(&header)),
        );
    }
    (disposition.disposition == DispositionType::Attachment).then(|| "attachment".into())
}

fn parts<'a>(
    mail: &'a ParsedMail<'a>,
    out: &mut Vec<(usize, String, &'a ParsedMail<'a>)>,
    plain: &mut Option<&'a ParsedMail<'a>>,
    html: &mut Option<&'a ParsedMail<'a>>,
    next: &mut usize,
) {
    *next += 1;
    if let Some(name) = filename(mail) {
        out.push((*next, name, mail));
        return;
    }
    match mail.ctype.mimetype.as_str() {
        "text/plain" => {
            plain.get_or_insert(mail);
        }
        "text/html" => {
            html.get_or_insert(mail);
        }
        _ => {}
    }
    for part in &mail.subparts {
        parts(part, out, plain, html, next);
    }
}

struct MessageParts<'a> {
    body: Option<&'a ParsedMail<'a>>,
    attachments: Vec<(usize, String, &'a ParsedMail<'a>)>,
}

fn message_parts<'a>(mail: &'a ParsedMail<'a>) -> MessageParts<'a> {
    let (mut plain, mut html) = (None, None);
    let mut attachments = Vec::new();
    let mut next = 0;
    parts(mail, &mut attachments, &mut plain, &mut html, &mut next);
    MessageParts {
        body: plain.or(html),
        attachments,
    }
}

fn parsed(bytes: &[u8]) -> Result<ParsedMail<'_>> {
    mailparse::parse_mail(bytes).map_err(|_| anyhow::anyhow!("メールの MIME 構造を解析できない"))
}

fn payload(part: &ParsedMail<'_>) -> Result<Vec<u8>> {
    part.get_body_raw()
        .map_err(|_| anyhow::anyhow!("メールの MIME パートを復号できない"))
}

fn text(part: &ParsedMail<'_>) -> Result<String> {
    let bytes = payload(part)?;
    let label = part.ctype.charset.as_bytes();
    let encoding = match label.to_ascii_lowercase().as_slice() {
        b"gb2312" | b"gbk" | b"x-gbk" => Some(GB18030),
        b"cp932" | b"windows-31j" => Some(SHIFT_JIS),
        b"us-ascii" => Some(UTF_8),
        _ => Encoding::for_label(label),
    };
    let encoding = encoding.unwrap_or(UTF_8);
    let (decoded, _, bad) = encoding.decode(&bytes);
    let value = if bad && part.ctype.charset.eq_ignore_ascii_case("us-ascii") {
        part.get_body().unwrap_or_else(|_| decoded.into_owned())
    } else {
        decoded.into_owned()
    };
    let value = if part.ctype.mimetype == "text/html" {
        html_to_text(&value)
    } else {
        value
    };
    static BLANK_LINES: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\n(?:[^\S\n]*\n)+").unwrap());
    let clean = strip_controls(&value.replace("\r\n", "\n"));
    Ok(shorten_tokens(&BLANK_LINES.replace_all(&clean, "\n\n")).into_owned())
}

fn strip_controls(input: &str) -> String {
    input
        .chars()
        .filter(|&ch| {
            matches!(ch, '\n' | '\t')
                || !ch.is_control()
                    && !matches!(ch,
                        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
                    )
        })
        .collect()
}

fn shorten_tokens(input: &str) -> Cow<'_, str> {
    const LONG_TOKEN: usize = 128;
    static TOKENS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i-u:https?)://[^\s<>"']+|[A-Za-z0-9+/_-]{128,}={0,2}"#).unwrap()
    });
    TOKENS.replace_all(input, |caps: &regex::Captures<'_>| {
        let token = &caps[0];
        if token.len() < LONG_TOKEN {
            return token.to_owned();
        }
        if let Ok(url) = Url::parse(token) {
            if let Some(host) = url.host_str() {
                return format!("{host}/…");
            }
        }
        // The token is either an overlong URL with no usable host or an opaque value.
        "…".into()
    })
}

fn clean_header(input: &str) -> String {
    let line = strip_controls(input).replace(['\n', '\t'], " ");
    shorten_tokens(&line).into_owned()
}

pub(super) fn wrap_header(key: &str, input: &str) -> String {
    wrap(key, &clean_header(input))
}

fn hides_element(tag: &StartTag) -> bool {
    if matches!(
        tag.name.as_slice(),
        b"head" | b"script" | b"style" | b"template"
    ) || tag.attributes.contains_key(b"hidden".as_slice())
    {
        return true;
    }
    let Some(style) = tag.attributes.get(b"style".as_slice()) else {
        return false;
    };
    static COMMENT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?s)/\*.*?\*/").unwrap());
    static ZERO: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^[-+]?(?:0+(?:\.0*)?|\.0+)(?:[a-z]+|%)?$").unwrap());
    let style = String::from_utf8_lossy(style).to_ascii_lowercase();
    COMMENT
        .replace_all(&style, "")
        .split(';')
        .any(|declaration| {
            let Some((name, value)) = declaration.split_once(':') else {
                return false;
            };
            let value = value.split('!').next().unwrap_or_default().trim();
            match name.trim() {
                "display" => value == "none",
                "visibility" => value == "hidden",
                "font-size" | "opacity" => ZERO.is_match(value),
                _ => false,
            }
        })
}

fn html_to_text(input: &str) -> String {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut hidden = Vec::new();
    let mut text = String::new();
    for token in Tokenizer::new_with_emitter(input, emitter).flatten() {
        match token {
            Token::StartTag(tag) => {
                if hidden.is_empty() && !hides_element(&tag) {
                    if tag.name == b"br" {
                        text.push('\n');
                    }
                } else if !matches!(
                    tag.name.as_slice(),
                    b"area"
                        | b"base"
                        | b"br"
                        | b"col"
                        | b"embed"
                        | b"hr"
                        | b"img"
                        | b"input"
                        | b"link"
                        | b"meta"
                        | b"param"
                        | b"source"
                        | b"track"
                        | b"wbr"
                ) {
                    hidden.push(tag.name);
                }
            }
            Token::EndTag(tag) if !hidden.is_empty() => {
                if let Some(index) = hidden.iter().rposition(|name| name == &tag.name) {
                    hidden.truncate(index);
                }
            }
            Token::String(content) if hidden.is_empty() => {
                text.push_str(&String::from_utf8_lossy(&content))
            }
            Token::EndTag(tag)
                if matches!(
                    tag.name.as_slice(),
                    b"p" | b"div" | b"li" | b"tr" | b"blockquote"
                ) =>
            {
                text.push('\n')
            }
            _ => {}
        }
    }
    text.trim().to_owned()
}

pub(super) fn read(raw: &RawMessage, max: usize) -> Result<Value> {
    let mail = parsed(&raw.bytes)?;
    let fields = Headers::from_headers(&mail.headers);
    let parts = message_parts(&mail);
    let key = &raw.key;
    let mut body = parts.body.map(text).transpose()?.unwrap_or_default();
    let omitted = body.chars().count().saturating_sub(max);
    if omitted > 0 {
        let end = body.char_indices().nth(max).unwrap().0;
        body.truncate(end);
        body.push_str(&format!("\n\n（以下 {omitted} 文字を省略）"));
    }
    let attachments = parts
        .attachments
        .into_iter()
        .map(|(part, filename, data)| {
            Ok(json!({
                "part": part, "filename": wrap_header(key, &filename),
                "mime": wrap_header(key, &data.ctype.mimetype), "size": payload(data)?.len(),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut value = headers_json(&raw.id, key, &fields, raw.seen);
    value["body"] = json!(wrap(key, &body));
    value["truncated"] = json!(omitted > 0);
    value["attachments"] = json!(attachments);
    Ok(value)
}

fn headers_json(id: &str, key: &str, fields: &Headers, seen: bool) -> Value {
    let tags = if seen { Vec::new() } else { vec!["未読"] };
    json!({
        "id": wrap(key, id),
        "from": wrap(key, &fields.display_from()),
        "sender_warning": fields.sender_warning(),
        "to": wrap_header(key, &fields.to),
        "cc": wrap_header(key, &fields.cc),
        "date": wrap_header(key, &fields.display_date()),
        "subject": wrap_header(key, &fields.subject),
        "message_id": wrap_header(key, &fields.message_id),
        "references": wrap_header(key, &fields.references),
        "in_reply_to": wrap_header(key, &fields.in_reply_to),
        "tags": tags,
    })
}

pub(super) fn withheld(id: &str, key: &str, fields: &Headers, seen: bool) -> Value {
    let mut value = headers_json(id, key, fields, seen);
    value["tags"].as_array_mut().unwrap().push(json!("査読"));
    value["body"] = Value::Null;
    value["attachments"] = Value::Null;
    value["body_withheld"] = json!(true);
    value
}

pub(super) fn body_text(bytes: &[u8]) -> Result<String> {
    message_parts(&parsed(bytes)?)
        .body
        .map(text)
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn attachment(bytes: &[u8], part: usize) -> Result<(String, Vec<u8>)> {
    let mail = parsed(bytes)?;
    let (_, name, data) = message_parts(&mail)
        .attachments
        .into_iter()
        .find(|a| a.0 == part)
        .context("指定した MIME パートは添付ファイルではない")?;
    Ok((name, payload(data)?))
}
