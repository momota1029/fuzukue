use std::time::Duration;

use anyhow::{ensure, Context, Result};
use async_imap::imap_proto::{BodyContentCommon, BodyStructure, MailboxDatum, Response, Status};
use async_imap::types::{Flag, NameAttribute};
use async_imap::{Authenticator, Client, Session};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use tokio_rustls::{client::TlsStream, TlsConnector};

use super::{
    auth::Secret, Account, ExaminedMailbox, Filter, Folder, Header, HeaderFetch, ImapTransport, Uid,
};

const TIMEOUT: Duration = Duration::from_secs(45);
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

pub(super) struct Transport<
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug = TlsStream<TcpStream>,
>(Session<T>);

impl Transport {
    pub(super) async fn connect(account: &Account, secret: Secret) -> Result<Self> {
        timeout(TIMEOUT, async {
            let host =
                ServerName::try_from(account.host.clone()).context("IMAP のホスト名が不正")?;
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let tcp = TcpStream::connect((account.host.as_str(), account.port))
                .await
                .map_err(|_| anyhow::anyhow!("IMAP サーバーに接続できない"))?;
            let stream = TlsConnector::from(std::sync::Arc::new(tls))
                .connect(host, tcp)
                .await
                .map_err(|_| anyhow::anyhow!("IMAP の TLS 接続に失敗"))?;
            Transport::authenticate(stream, &account.user, secret).await
        })
        .await
        .context("IMAP 接続が時間切れ")?
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Transport<T> {
    async fn authenticate(stream: T, user: &str, secret: Secret) -> Result<Self> {
        let mut client = Client::new(stream);
        let greeting = client
            .read_response()
            .await
            .ok()
            .flatten()
            .context("IMAP の応答がない")?;
        ensure!(
            matches!(
                greeting.parsed(),
                Response::Data {
                    status: Status::Ok,
                    ..
                }
            ),
            "IMAP サーバーが接続を受け付けなかった"
        );
        let session = match secret {
            Secret::Password(password) => {
                ensure!(
                    !password.chars().any(char::is_control),
                    "IMAP パスワードの形式が不正"
                );
                client.login(user, password).await
            }
            Secret::OAuth2(token) => {
                client
                    .authenticate(
                        "XOAUTH2",
                        Xoauth2(Some(format!("user={user}\x01auth=Bearer {token}\x01\x01"))),
                    )
                    .await
            }
        }
        .map_err(|_| anyhow::anyhow!("IMAP 認証に失敗。利用者が mail auth を確認する"))?;
        Ok(Self(session))
    }
}

struct Xoauth2(Option<String>);

impl Authenticator for Xoauth2 {
    type Response = String;

    fn process(&mut self, _: &[u8]) -> String {
        // An OAuth error challenge gets an empty response, never a second token.
        self.0.take().unwrap_or_default()
    }
}

pub(super) struct Mailbox<'a, T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug>(
    &'a mut Session<T>,
    u32,
);

impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> ImapTransport for Transport<T> {
    type Mailbox<'a>
        = Mailbox<'a, T>
    where
        Self: 'a;

    async fn list(&mut self) -> Result<Vec<Folder>> {
        timeout(TIMEOUT, async {
            let mut names = self.0.list(None, Some("\"*\"")).await?;
            let mut folders = Vec::new();
            while let Some(name) = names.try_next().await? {
                if name.attributes().contains(&NameAttribute::NoSelect) {
                    continue;
                }
                let special_use = name
                    .attributes()
                    .iter()
                    .filter_map(|attr| {
                        Some(
                            match attr {
                                NameAttribute::All => "\\All",
                                NameAttribute::Archive => "\\Archive",
                                NameAttribute::Drafts => "\\Drafts",
                                NameAttribute::Flagged => "\\Flagged",
                                NameAttribute::Junk => "\\Junk",
                                NameAttribute::Sent => "\\Sent",
                                NameAttribute::Trash => "\\Trash",
                                _ => return None,
                            }
                            .to_owned(),
                        )
                    })
                    .collect();
                let Ok(display) = decode_folder(name.name()) else {
                    continue;
                };
                folders.push(Folder {
                    name: display,
                    wire: name.name().into(),
                    special_use,
                });
            }
            Ok::<_, async_imap::error::Error>(folders)
        })
        .await
        .context("IMAP LIST が時間切れ")?
        .map_err(|_| anyhow::anyhow!("IMAP LIST に失敗"))
    }

    async fn examine(&mut self, wire: &str) -> Result<Self::Mailbox<'_>> {
        ensure!(
            !wire.is_empty() && !wire.chars().any(char::is_control),
            "IMAP フォルダー名が不正"
        );
        let folder = timeout(TIMEOUT, self.0.examine(wire))
            .await
            .context("IMAP EXAMINE が時間切れ")?
            .map_err(|_| anyhow::anyhow!("IMAP EXAMINE に失敗"))?;
        let validity = folder
            .uid_validity
            .context("IMAP フォルダーの UIDVALIDITY が不明")?;
        Ok(Mailbox(&mut self.0, validity))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> ExaminedMailbox
    for Mailbox<'_, T>
{
    fn uid_validity(&self) -> u32 {
        self.1
    }

    async fn uid_search(&mut self, filters: &[Filter]) -> Result<Vec<Uid>> {
        let query = search_query(filters)?;
        timeout(TIMEOUT, run_search(self.0, &query))
            .await
            .context("IMAP SEARCH が時間切れ")?
            .map_err(|_| anyhow::anyhow!("IMAP SEARCH に失敗"))
    }

    async fn peek_headers(&mut self, uids: &[Uid], fields: HeaderFetch) -> Result<Vec<Header>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let query = match fields {
            HeaderFetch::Summary => "(UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)])",
            HeaderFetch::Structure => "(UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)] BODYSTRUCTURE)",
            HeaderFetch::Thread => "(UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES AUTHENTICATION-RESULTS)])",
        };
        timeout(TIMEOUT, async {
            let mut fetches = self.0.uid_fetch(set, query).await?;
            let mut headers = Vec::new();
            while let Some(fetch) = fetches.try_next().await? {
                let Some(uid) = fetch
                    .uid
                    .and_then(Uid::new)
                    .filter(|uid| uids.contains(uid))
                else {
                    continue;
                };
                if let Some(bytes) = fetch.header() {
                    headers.push(Header {
                        uid,
                        seen: fetch.flags().any(|flag| flag == Flag::Seen),
                        bytes: bytes.to_vec(),
                        has_attachment: fetch.bodystructure().map(has_attachment),
                    });
                }
            }
            Ok::<_, async_imap::error::Error>(headers)
        })
        .await
        .context("IMAP ヘッダー取得が時間切れ")?
        .map_err(|_| anyhow::anyhow!("IMAP ヘッダーを取得できない"))
    }

    async fn peek_message(&mut self, uid: Uid) -> Result<Vec<u8>> {
        // One extra byte distinguishes a complete message from an oversized prefix.
        let query = format!("(UID BODY.PEEK[]<0.{}>)", MAX_MESSAGE + 1);
        let bytes = timeout(TIMEOUT, async {
            let mut fetches = self.0.uid_fetch(uid.to_string(), query).await?;
            let mut bytes = None;
            while let Some(fetch) = fetches.try_next().await? {
                if fetch.uid == Some(uid.get()) {
                    if let Some(body) = fetch.body() {
                        bytes = Some(body.to_vec());
                    }
                }
            }
            Ok::<_, async_imap::error::Error>(bytes)
        })
        .await
        .context("IMAP 本文取得が時間切れ")?
        .map_err(|_| anyhow::anyhow!("IMAP 本文を取得できない"))?
        .context("指定されたメールが見つからない")?;
        ensure!(
            bytes.len() <= MAX_MESSAGE,
            "メールが取得上限の 64 MiB を超えている"
        );
        Ok(bytes)
    }
}

fn has_attachment(body: &BodyStructure<'_>) -> bool {
    fn named(part: &BodyContentCommon<'_>) -> bool {
        part.disposition.as_ref().is_some_and(|disposition| {
            disposition.ty.eq_ignore_ascii_case("attachment")
                || disposition
                    .params
                    .iter()
                    .flatten()
                    .any(|(key, _)| key.to_ascii_lowercase().starts_with("filename"))
        }) || part.ty.params.iter().flatten().any(|(key, _)| {
            let key = key.to_ascii_lowercase();
            key == "name" || key.starts_with("name*")
        })
    }
    match body {
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. } => named(common),
        BodyStructure::Multipart { common, bodies, .. } => {
            named(common) || bodies.iter().any(has_attachment)
        }
    }
}

async fn run_search<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug>(
    session: &mut Session<T>,
    query: &str,
) -> Result<Vec<Uid>> {
    // async-imap parses responses and frames the command. Use its lower-level
    // API for SEARCH to handle synchronizing literals and check tagged status.
    let mut segments = query.split("\r\n");
    let first = segments.next().context("SEARCH 条件がない")?;
    let tag = session.run_command(format!("UID SEARCH {first}")).await?;
    for segment in segments {
        loop {
            let response = session
                .read_response()
                .await?
                .context("SEARCH 応答がない")?;
            match response.parsed() {
                Response::Continue(_) => break,
                Response::Done { .. }
                | Response::Data {
                    status: Status::Bye,
                    ..
                } => {
                    anyhow::bail!("SEARCH リテラルを拒否された");
                }
                _ => {}
            }
        }
        session.run_command_untagged(segment).await?;
    }
    let mut uids = std::collections::BTreeSet::new();
    loop {
        let response = session
            .read_response()
            .await?
            .context("SEARCH が完了しなかった")?;
        match response.parsed() {
            Response::MailboxData(MailboxDatum::Search(results)) => {
                uids.extend(results.iter().copied().filter_map(Uid::new));
            }
            Response::Done {
                tag: response_tag,
                status,
                ..
            } if *response_tag == tag => {
                ensure!(*status == Status::Ok, "SEARCH が拒否された");
                return Ok(uids.into_iter().collect());
            }
            Response::Data {
                status: Status::Bye,
                ..
            } => anyhow::bail!("IMAP が切断された"),
            _ => {}
        }
    }
}

fn quoted(input: &str) -> Result<String> {
    ensure!(
        input.len() <= 4096 && !input.chars().any(char::is_control),
        "IMAP の検索条件が不正"
    );
    if !input.is_ascii() {
        return Ok(format!("{{{}}}\r\n{input}", input.len()));
    }
    Ok(format!(
        "\"{}\"",
        input.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn search_query(filters: &[Filter]) -> Result<String> {
    if filters.is_empty() {
        return Ok("ALL".into());
    }
    let mut utf8 = false;
    let mut parts = Vec::new();
    for filter in filters {
        let pair = match filter {
            Filter::Since(date) => format!("SINCE {}", date.format("%d-%b-%Y")),
            Filter::Before(date) => format!("BEFORE {}", date.format("%d-%b-%Y")),
            Filter::From(text) => format!("FROM {}", quoted(text)?),
            Filter::FromAny(senders) => {
                ensure!(!senders.is_empty(), "送信元の検索条件が空");
                let operands = senders
                    .iter()
                    .map(|s| Ok(format!("FROM {}", quoted(s)?)))
                    .collect::<Result<Vec<_>>>()?
                    .join(" ");
                format!("{}{operands}", "OR ".repeat(senders.len() - 1))
            }
            Filter::Subject(text) => format!("SUBJECT {}", quoted(text)?),
            Filter::Text(text) => format!("TEXT {}", quoted(text)?),
            Filter::Unseen => "UNSEEN".into(),
            Filter::GmailRaw(text) => {
                utf8 = true;
                format!("X-GM-RAW {}", quoted(text)?)
            }
            Filter::ThreadIds(ids) => {
                ensure!(!ids.is_empty(), "スレッドの検索 ID が空");
                let terms = ids.iter().map(|id| {
                    let id = quoted(id)?;
                    Ok(format!("OR OR HEADER \"Message-ID\" {id} HEADER \"References\" {id} HEADER \"In-Reply-To\" {id}"))
                }).collect::<Result<Vec<_>>>()?;
                format!("{}{}", "OR ".repeat(terms.len() - 1), terms.join(" "))
            }
        };
        utf8 |= !pair.is_ascii();
        parts.push(pair);
    }
    let prefix = if utf8 { "CHARSET UTF-8 " } else { "" };
    Ok(format!("{prefix}{}", parts.join(" ")))
}

pub(super) fn encode_folder(name: &str) -> Result<String> {
    ensure!(
        name.len() <= 4096 && !name.is_empty() && !name.chars().any(char::is_control),
        "フォルダー名が不正"
    );
    fn flush(out: &mut String, bytes: &mut Vec<u8>) {
        if !bytes.is_empty() {
            out.push('&');
            out.push_str(&STANDARD_NO_PAD.encode(bytes.as_slice()).replace('/', ","));
            out.push('-');
            bytes.clear();
        }
    }
    let mut out = String::new();
    let mut bytes = Vec::new();
    for unit in name.encode_utf16() {
        if (0x20..=0x7e).contains(&unit) {
            flush(&mut out, &mut bytes);
            if unit == b'&' as u16 {
                out.push_str("&-");
            } else {
                out.push(char::from(unit as u8));
            }
        } else {
            bytes.extend(unit.to_be_bytes());
        }
    }
    flush(&mut out, &mut bytes);
    Ok(out)
}

fn decode_folder(mut name: &str) -> Result<String> {
    let mut out = String::new();
    while let Some((before, rest)) = name.split_once('&') {
        out.push_str(before);
        let (encoded, after) = rest.split_once('-').context("modified UTF-7 が不正")?;
        if encoded.is_empty() {
            out.push('&');
        } else {
            let bytes = STANDARD_NO_PAD.decode(encoded.replace(',', "/"))?;
            ensure!(bytes.len() % 2 == 0, "modified UTF-7 が不正");
            let units = bytes
                .chunks_exact(2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .collect::<Vec<_>>();
            out.push_str(&String::from_utf16(&units)?);
        }
        name = after;
    }
    out.push_str(name);
    ensure!(
        !out.chars().any(char::is_control),
        "IMAP フォルダー名が不正"
    );
    Ok(out)
}

#[cfg(test)]
mod tests;
