//! Scrub every submitted text field before it is fingerprinted or persisted.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use anyhow::{Result, ensure};
use regex::{Captures, Regex};

const REDACTED: &str = "<redacted>";

static REDACTIONS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    // Match quoted paths first so home paths containing spaces are removed in full.
    let unix_home = r"/(?:home|Users|var/home|mnt/[a-z]/Users)/|/root\b";
    let windows_home = r"[a-z]:[/\\]+(?:Users|Documents[ ]and[ ]Settings)[/\\]+";
    let home = format!(r"(?i:{unix_home}|{windows_home})");
    let patterns = [
        (format!(r#""{home}(?:\\[^\r\n]|[^"\\\r\n])*"|'{home}[^'\r\n]*'"#), "<home-path>"),
        (format!(r#"{home}(?:\\[^\r\n]|[^\s"'<>|;&\x60\\()\[\]{{}},])*"#), "<home-path>"),
        (r#"(?i)(\b[a-z][a-z0-9+.-]*://)[^\s/<>'"]+@"#.into(), "$1<redacted>@"),
        (r#"(?i)(?:"(?:proxy-authorization|authorization|set-cookie|cookie)\s*:(?:\\[^\r\n]|[^"\\\r\n])*"|'(?:proxy-authorization|authorization|set-cookie|cookie)\s*:[^'\r\n]*')"#.into(), REDACTED),
        (r#"(?i)\b(?:authorization|proxy-authorization)\s*:\s*(?:bearer|basic|token|digest)\s+[^ \t\r\n"'<>|;]+"#.into(), REDACTED),
        (r#"(?i)(\b[\w-]*(?:api[-_]?key|access[-_]?key|private[-_]?key|client[-_]?secret|token|password|passwd|secret|cookie)[\w-]*["']?\s*(?:=|:)\s*|--[\w-]*(?:key|token|password|passwd|secret|cookie)[\w-]*[ \t]+)(?:"(?:\\[^\r\n]|[^"\\\r\n])*"|'[^'\r\n]*'|[^\s"'\x60&;|<>),}\]]+)"#.into(), "$1<redacted>"),
        (r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+|glpat-[A-Za-z0-9_-]+|xox[baprs]-[A-Za-z0-9-]+|sk-(?:proj-|ant-)?[A-Za-z0-9_-]{12,}|hf_[A-Za-z0-9]{12,}|(?:AKIA|ASIA)[A-Z0-9]{16}|AIza[A-Za-z0-9_-]{30,}|tskey-[A-Za-z0-9_-]+|eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*)\b".into(), REDACTED),
        (r"[\p{L}\p{N}._%+'-]+@(?:[\p{L}\p{N}-]+\.)+[\p{L}]{2,}".into(), "<email>"),
    ];
    patterns
        .into_iter()
        .map(|(pattern, replacement)| (Regex::new(&pattern).unwrap(), replacement))
        .collect()
});

static PRIVATE_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:[\p{L}\p{N}_-]+\.)*(?:[\p{L}\p{N}_-]+\.ts\.net|[\p{L}\p{N}_-]+\.(?:local|lan|internal|localhost|localdomain|corp)|localhost)\b\.?").unwrap()
});
static IP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?<v6>\b{start-half}(?:[a-f0-9]*:){2,}[a-f0-9:.]*(?:%[\w.-]+)?\b{end-half})|[0-9]{1,3}(?:\.[0-9]{1,3}){3}",
    )
    .unwrap()
});
static KEY_MATERIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:begin|end)\b[^\r\n]{0,50}\bprivate[ _-]+key\b|\bopenssh-key-v1\b|\bssh-(?:rsa|ed25519)\s+AAAA").unwrap()
});
static AMBIGUOUS_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:(?:\b(?:proxy-authorization|authorization|set-cookie|cookie)\s*:\s*|\bbearer\s+)(?:["']?[^\s"'<>]+)|\b[\w-]*(?:api[-_]?key|access[-_]?key|private[-_]?key|client[-_]?secret|token|password|passwd|secret|cookie)[\w-]*["']?\s*(?:=|:)\s*["'\x60\\])"#).unwrap()
});
static OPAQUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_/+=-]{24,}").unwrap());

pub(in crate::mcp) fn field(name: &str, input: &str, max: usize, required: bool) -> Result<String> {
    sanitize_field(name, input, max, required, true)
}

pub(in crate::mcp) fn patch_line_has_private_data(input: &str, max: usize) -> bool {
    // Loopback literals and localhost are common in source code and test fixtures.
    !sanitize_field("added line", input, max, false, false)
        .is_ok_and(|sanitized| sanitized == input)
}

fn sanitize_field(
    name: &str,
    input: &str,
    max: usize,
    required: bool,
    redact_loopback: bool,
) -> Result<String> {
    ensure!(input.len() <= max, "{name} exceeds {max} UTF-8 bytes");
    ensure!(
        !required || !input.trim().is_empty(),
        "{name} must be nonempty"
    );
    ensure!(
        !input.chars().any(|c| {
            c.is_control() && !matches!(c, '\t' | '\n' | '\r')
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        }),
        "{name} contains a control or invisible character"
    );
    ensure!(
        !KEY_MATERIAL.is_match(input),
        "{name} contains possible key material. Replace it with a placeholder and retry."
    );
    let mut text = input.replace("\r\n", "\n");
    ensure!(
        !text.contains('\r'),
        "{name} contains a bare carriage return"
    );
    for (pattern, replacement) in REDACTIONS.iter() {
        text = pattern.replace_all(&text, *replacement).into_owned();
    }
    text = PRIVATE_HOST
        .replace_all(&text, |caps: &Captures| {
            let host = &caps[0];
            if !redact_loopback && host.trim_end_matches('.').eq_ignore_ascii_case("localhost") {
                host.to_owned()
            } else {
                "<private-host>".to_owned()
            }
        })
        .into_owned();
    text = IP
        .replace_all(&text, |caps: &Captures| {
            let found = caps.get(0).unwrap();
            let trimmed = found.as_str().trim_end_matches('.');
            let private = if caps.name("v6").is_some() {
                // Word boundaries can leave a partial match inside a colon-delimited token.
                let whole_token =
                    !text[..found.start()].ends_with(':') && !text[found.end()..].starts_with(':');
                whole_token && private_v6(trimmed, redact_loopback)
            } else {
                trimmed
                    .parse::<Ipv4Addr>()
                    .is_ok_and(|ip| private_v4(ip, redact_loopback))
            };
            if private {
                format!("<private-ip>{}", &found.as_str()[trimmed.len()..])
            } else {
                found.as_str().to_owned()
            }
        })
        .into_owned();
    ensure!(
        !AMBIGUOUS_SECRET.is_match(&text)
            && !OPAQUE
                .find_iter(&text)
                .any(|part| looks_opaque(part.as_str())),
        "{name} still contains possible credentials after redaction. Replace them with placeholders and retry."
    );
    Ok(text)
}

fn private_v6(token: &str, redact_loopback: bool) -> bool {
    let address = token.split_once('%').map_or(token, |(address, _)| address);
    // A single alphabetic group or bare `::` is too ambiguous in source code.
    let plausible = address.bytes().any(|byte| byte.is_ascii_digit())
        || address
            .split(':')
            .filter(|group| !group.is_empty())
            .nth(1)
            .is_some();
    plausible
        && address.parse::<Ipv6Addr>().is_ok_and(|ip| {
            if ip.is_loopback() {
                return redact_loopback;
            }
            ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4()
                    .is_some_and(|ip| private_v4(ip, redact_loopback))
        })
}

fn private_v4(ip: Ipv4Addr, redact_loopback: bool) -> bool {
    let [a, b, _, _] = ip.octets();
    ip.is_private()
        || (redact_loopback && ip.is_loopback())
        || ip.is_link_local()
        || a == 0
        || a == 100 && (64..=127).contains(&b)
}

fn looks_opaque(word: &str) -> bool {
    word.bytes().all(|b| b.is_ascii_hexdigit())
        || word.len() >= 32
            && word.bytes().any(|b| b.is_ascii_digit())
            && word.bytes().any(|b| b.is_ascii_lowercase())
            && word
                .bytes()
                .any(|b| b.is_ascii_uppercase() || b"+/=".contains(&b))
}
