use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use mailparse::{addrparse, msgidparse, MailAddr, SingleInfo};
use rustix::fs::{linkat, mkdirat, open, openat, unlinkat, AtFlags, Mode, OFlags};
use rustix::io::Errno;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use super::{auth::nonce, mime, mime::Headers, now, unwrap_mail, RawMessage};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const MAX_ATTACH: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    pub(super) account: String,
    to: String,
    cc: Option<String>,
    subject: String,
    body: String,
    pub(super) in_reply_to_id: Option<String>,
    #[serde(default)]
    attach: Vec<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launcher {
    executable: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Platform {
    Linux,
    Mac,
    Wsl,
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if std::env::var_os("WSL_INTEROP").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
        {
            Self::Wsl
        } else {
            Self::Linux
        }
    }
}

struct DraftFile {
    file: PathBuf,
    filename: String,
    data: Vec<u8>,
}

pub(super) fn save_attachment(
    project: &Path,
    deck: Option<&str>,
    fields: &Headers,
    name: &str,
    data: &[u8],
) -> Result<PathBuf> {
    let deck = deck.map(Path::new).unwrap_or(Path::new(""));
    if deck != Path::new(".") && !deck.as_os_str().is_empty() {
        directory(project, deck, false).context("deck はプロジェクト内のディレクトリで指定する")?;
    }
    let date = fields.when().unwrap_or_else(now).format("%Y-%m-%d");
    let subject = safe_name(&fields.subject, false, "mail");
    let directory = deck.join("inbox").join(format!("{date}-{subject}"));
    store_file(
        project,
        &directory,
        &safe_name(name, true, "attachment"),
        data,
    )
}

fn relative(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
}

fn directory(project: &Path, path: &Path, create: bool) -> Result<File> {
    ensure!(
        path.as_os_str().is_empty() || path == Path::new(".") || relative(path),
        "プロジェクト相対のパスが不正"
    );
    let mut dir = File::from(open(project, DIR_FLAGS, Mode::empty())?);
    if path == Path::new(".") {
        return Ok(dir);
    }
    for part in path {
        if create {
            match mkdirat(&dir, part, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        dir = File::from(openat(&dir, part, DIR_FLAGS, Mode::empty())?);
    }
    Ok(dir)
}

fn safe_name(text: &str, filename: bool, fallback: &str) -> String {
    let text = if filename {
        text.rsplit(['/', '\\']).next().unwrap_or_default()
    } else {
        text
    };
    let mut out = String::new();
    for c in text.chars() {
        let c = if c.is_alphanumeric() || matches!(c, '-' | '_') || (filename && c == '.') {
            c
        } else {
            '-'
        };
        if out.len() + c.len_utf8() > 140 {
            break;
        }
        if c != '-' || !out.ends_with('-') {
            out.push(c);
        }
    }
    let out = out.trim_matches(['.', '-', ' ']);
    if out.is_empty() {
        return fallback.into();
    }
    let base = out.split('.').next().unwrap_or(out).to_ascii_uppercase();
    let dos_device = ["CON", "PRN", "AUX", "NUL"].contains(&base.as_str())
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit());
    if dos_device {
        format!("_{out}")
    } else {
        out.into()
    }
}

fn store_file(project: &Path, path: &Path, filename: &str, data: &[u8]) -> Result<PathBuf> {
    let dir = directory(project, path, true)?;
    let temporary = format!(".mail-{}.tmp", nonce());
    let mut file = File::from(openat(
        &dir,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        let (stem, ext) = filename
            .rsplit_once('.')
            .map_or((filename, String::new()), |(stem, ext)| {
                (stem, format!(".{ext}"))
            });
        for i in 1..=10_000 {
            let name = if i == 1 {
                filename.into()
            } else {
                format!("{stem}-{i}{ext}")
            };
            match linkat(
                &dir,
                temporary.as_str(),
                &dir,
                name.as_str(),
                AtFlags::empty(),
            ) {
                Ok(()) => return Ok(path.join(name)),
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("保存先の同名ファイルが多すぎる")
    })();
    let _ = unlinkat(&dir, temporary.as_str(), AtFlags::empty());
    result
}

fn project_files(project: &Path, paths: &[String]) -> Result<Vec<DraftFile>> {
    let mut bytes_left = MAX_ATTACH;
    let mut files = Vec::new();
    for path in paths {
        let relative_path = unwrap_mail(path)?;
        let path = Path::new(&relative_path);
        ensure!(relative(path), "添付はプロジェクト相対のファイルで指定する");
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("添付ファイル名が不正")?;
        let dir = directory(project, path.parent().unwrap_or(Path::new("")), false)?;
        let file = File::from(openat(
            &dir,
            filename,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() <= bytes_left,
            "添付は通常ファイルのみ、合計 32 MiB まで"
        );
        let mut data = Vec::new();
        file.take(bytes_left + 1).read_to_end(&mut data)?;
        bytes_left = bytes_left
            .checked_sub(data.len() as u64)
            .context("添付は合計 32 MiB まで")?;
        files.push(DraftFile {
            file: project.join(path),
            filename: safe_name(filename, true, "attachment"),
            data,
        });
    }
    Ok(files)
}

fn encoded_word(text: &str) -> String {
    let mut words = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.floor_char_boundary(rest.len().min(42));
        words.push(format!(
            "=?UTF-8?B?{}?=",
            STANDARD.encode(&rest.as_bytes()[..end])
        ));
        rest = &rest[end..];
    }
    words.join("\r\n ")
}

fn mailbox(address: &SingleInfo) -> Result<String> {
    ensure!(
        !address.addr.is_empty()
            && address.addr.is_ascii()
            && address.addr.contains('@')
            && !address
                .addr
                .bytes()
                .any(|c| c.is_ascii_control() || b"<>".contains(&c)),
        "下書きのアドレスが不正"
    );
    Ok(
        match address
            .display_name
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            Some(name) => format!("{} <{}>", encoded_word(name), address.addr),
            None => format!("<{}>", address.addr),
        },
    )
}

fn addresses(input: &str) -> Result<String> {
    ensure!(
        !input.chars().any(char::is_control) && input.len() <= 8192,
        "下書きの宛先が不正"
    );
    let list = addrparse(input).map_err(|_| anyhow::anyhow!("下書きの宛先を解析できない"))?;
    ensure!(list.count_addrs() > 0, "下書きの宛先が空");
    let values = list
        .iter()
        .map(|address| match address {
            MailAddr::Single(single) => mailbox(single),
            MailAddr::Group(group) => Ok(format!(
                "{}: {};",
                encoded_word(&group.group_name),
                group
                    .addrs
                    .iter()
                    .map(mailbox)
                    .collect::<Result<Vec<_>>>()?
                    .join(",\r\n ")
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(values.join(",\r\n "))
}

pub(super) fn msgids(value: &str) -> Vec<String> {
    let Ok(ids) = msgidparse(value) else {
        return Vec::new();
    };
    ids.iter()
        .filter(|id| {
            id.len() < 198
                && id.is_ascii()
                && id
                    .split_once('@')
                    .is_some_and(|(local, host)| !local.is_empty() && !host.is_empty())
                && !id
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"<>".contains(&b))
        })
        .map(|id| format!("<{id}>"))
        .collect()
}

pub(super) struct Reply {
    parent: Option<String>,
    references: Vec<String>,
    quote: Option<String>,
}

pub(super) fn reply(raw: &RawMessage) -> Result<Reply> {
    let fields = Headers::parse(&raw.bytes)?;
    let parent = msgids(&fields.message_id).into_iter().next();
    let mut references = msgids(&fields.references);
    if references.is_empty() {
        let previous = msgids(&fields.in_reply_to);
        if previous.len() == 1 {
            references = previous;
        }
    }
    if let Some(parent) = &parent {
        if references.last() != Some(parent) {
            references.push(parent.clone());
        }
    }
    if references.len() > 64 {
        references.drain(1..references.len() - 63);
    }

    let quote = if parent.is_none() {
        let text = mime::body_text(&raw.bytes)?;
        let end = text
            .char_indices()
            .nth(1_500)
            .map_or(text.len(), |(end, _)| end);
        let summary = format!("{} — {}", fields.display_from(), fields.subject);
        let summary_end = summary
            .char_indices()
            .nth(300)
            .map_or(summary.len(), |(end, _)| end);
        let mut quote = format!("\n\n> {}\n", &summary[..summary_end]);
        for line in text[..end].lines() {
            writeln!(quote, "> {line}")?;
        }
        if end < text.len() {
            quote.push_str("> […]\n");
        }
        Some(quote.replace('\0', "�"))
    } else {
        None
    };
    Ok(Reply {
        parent,
        references,
        quote,
    })
}

fn write_base64(out: &mut Vec<u8>, data: &[u8]) {
    for line in data.chunks(57) {
        out.extend(STANDARD.encode(line).as_bytes());
        out.extend(b"\r\n");
    }
}

fn body(out: &mut Vec<u8>, text: &str) -> Result<()> {
    write!(
        out,
        "Content-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n"
    )?;
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n");
    write_base64(out, text.as_bytes());
    Ok(())
}

pub(super) fn write(
    project: &Path,
    user: &str,
    mut args: Args,
    reply: Option<Reply>,
    launcher: Option<&Launcher>,
) -> Result<Value> {
    args.to = unwrap_mail(&args.to)?;
    args.cc = args.cc.as_deref().map(unwrap_mail).transpose()?;
    args.subject = unwrap_mail(&args.subject)?;
    let to = addresses(&args.to)?;
    let cc = args
        .cc
        .as_deref()
        .filter(|cc| !cc.is_empty())
        .map(addresses)
        .transpose()?;
    let from = addrparse(user)
        .ok()
        .and_then(|list| list.extract_single_info())
        .context("アカウントの user を差出人アドレスとして使えない")?;
    let from = mailbox(&from)?;
    ensure!(
        !args.subject.chars().any(char::is_control) && args.subject.len() <= 8192,
        "下書きの件名が不正"
    );
    ensure!(
        args.body.len() <= 2 * 1024 * 1024 && !args.body.contains('\0'),
        "下書き本文が長すぎるか不正"
    );
    let files = project_files(project, &args.attach)?;
    let mut message = Vec::new();
    let now = now();
    write!(
        message,
        "Date: {}\r\nFrom: {from}\r\nTo: {to}\r\n",
        now.to_rfc2822()
    )?;
    if let Some(cc) = cc {
        write!(message, "Cc: {cc}\r\n")?;
    }
    write!(
        message,
        "Subject: {}\r\nMessage-ID: <{}@labo.invalid>\r\nMIME-Version: 1.0\r\nX-Unsent: 1\r\n",
        encoded_word(&args.subject),
        nonce()
    )?;
    if let Some(reply) = &reply {
        if let Some(parent) = &reply.parent {
            write!(
                message,
                "In-Reply-To: {parent}\r\nReferences: {}\r\n",
                reply.references.join("\r\n ")
            )?;
        }
        if let Some(quote) = &reply.quote {
            args.body.push_str(quote);
        }
    }
    if files.is_empty() {
        body(&mut message, &args.body)?;
    } else {
        let boundary = format!("labo-{}", nonce());
        write!(
            message,
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n"
        )?;
        body(&mut message, &args.body)?;
        for file in &files {
            let filename = percent(&file.filename);
            write!(message, "--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment;\r\n filename*=UTF-8''{filename}\r\n\r\n")?;
            write_base64(&mut message, &file.data);
        }
        write!(message, "--{boundary}--\r\n")?;
    }
    let name = format!("{}-draft-{}.eml", now.format("%Y-%m-%d"), &nonce()[..12]);
    let path = store_file(project, Path::new("outbox"), &name, &message)?;
    // Trial — may be removed. The child process only opens the compose UI.
    let draft = reply
        .as_ref()
        .filter(|reply| reply.parent.is_some())
        .map(|_| project.join(&path));
    let mut notices = Vec::new();
    let started = launcher.is_some_and(|launcher| {
        launcher
            .launch(user, &args, &files, draft.as_deref())
            .is_ok()
    });
    let threading = match (started, &reply) {
        (true, Some(reply)) if reply.quote.is_some() => Some("quoted"),
        (true, Some(_)) => Some("eml"),
        _ => None,
    };
    if launcher.is_some() && !started {
        notices.push("Thunderbird を起動できない。保存済みの .eml を利用者が開く");
    }
    if reply.as_ref().is_some_and(|reply| reply.quote.is_some()) {
        notices.push("有効な返信元 Message-ID がないため、返信元を本文に引用した");
    }
    Ok(json!({
        "path": path, "compose_started": started, "compose_threading": threading,
        "notice": (!notices.is_empty()).then(|| notices.join("。")),
    }))
}

fn percent(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

impl Launcher {
    fn command(
        &self,
        platform: Platform,
        user: &str,
        args: &Args,
        files: &[DraftFile],
        draft: Option<&Path>,
        convert: impl Fn(&Path) -> Result<String>,
    ) -> Result<Command> {
        let mut command = match (&self.executable, platform) {
            (Some(executable), _) => Command::new(executable),
            (None, Platform::Linux) => Command::new("thunderbird"),
            (None, Platform::Mac) => {
                let mut command = Command::new("open");
                command.args(["-a", "Thunderbird", "--args"]);
                command
            }
            (None, Platform::Wsl) => {
                let path = [
                    "/mnt/c/Program Files/Mozilla Thunderbird/thunderbird.exe",
                    "/mnt/c/Program Files (x86)/Mozilla Thunderbird/thunderbird.exe",
                ]
                .into_iter()
                .find(|path| Path::new(path).is_file())
                .context("Windows の Thunderbird が見つからない")?;
                Command::new(path)
            }
        };
        if let Some(draft) = draft {
            // Thunderbird opens an X-Unsent EML as an editable draft and loads References.
            command.arg("-file");
            if matches!(platform, Platform::Wsl) {
                command.arg(convert(draft)?);
            } else {
                command.arg(draft);
            }
        } else {
            let compose = compose_options(platform, user, args, files, convert)?;
            ensure!(compose.len() <= 24_000, "Thunderbird の起動引数が長すぎる");
            command.args(["-compose", &compose]);
        }
        Ok(command)
    }

    fn launch(
        &self,
        user: &str,
        args: &Args,
        files: &[DraftFile],
        draft: Option<&Path>,
    ) -> Result<()> {
        let mut command = self.command(Platform::current(), user, args, files, draft, |path| {
            let output = Command::new("wslpath").arg("-w").arg(path).output()?;
            ensure!(output.status.success(), "wslpath の変換に失敗");
            Ok(String::from_utf8(output.stdout)?
                .trim_end_matches(['\r', '\n'])
                .into())
        })?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

fn compose_options(
    platform: Platform,
    user: &str,
    args: &Args,
    files: &[DraftFile],
    convert: impl Fn(&Path) -> Result<String>,
) -> Result<String> {
    // Unquoted values are percent-decoded by Thunderbird after it splits the fields.
    let mut opts = vec![
        format!("from={}", percent(user)),
        format!("to={}", percent(&args.to)),
    ];
    if let Some(cc) = args.cc.as_deref() {
        opts.push(format!("cc={}", percent(cc)));
    }
    opts.push(format!("subject={}", percent(&args.subject)));
    opts.push(format!("body={}", percent(&args.body)));
    opts.push("format=text".into());
    if !files.is_empty() {
        let urls = files
            .iter()
            .map(|file| {
                let url = if matches!(platform, Platform::Wsl) {
                    windows_url(&convert(&file.file)?)?
                } else {
                    Url::from_file_path(&file.file)
                        .map_err(|_| anyhow::anyhow!("添付ファイル URL の変換に失敗"))?
                };
                Ok(url.as_str().replace(',', "%2C").replace('\'', "%27"))
            })
            .collect::<Result<Vec<_>>>()?;
        // Quotes preserve the URI's own percent-encoding and the attachment separator.
        opts.push(format!("attachment='{}'", urls.join(",")));
    }
    Ok(opts.join(","))
}

fn windows_url(windows: &str) -> Result<Url> {
    let path = windows.replace('\\', "/");
    let (mut url, rest) = if let Some(unc) = path.strip_prefix("//") {
        let (host, rest) = unc.split_once('/').context("WSL の UNC パスが不正")?;
        let mut url = Url::parse("file:///")?;
        url.set_host(Some(host))
            .map_err(|_| anyhow::anyhow!("WSL の UNC ホストが不正"))?;
        (url, rest)
    } else {
        let (drive, rest) = path.split_once('/').context("Windows パスが不正")?;
        ensure!(
            drive.len() == 2 && drive.as_bytes()[0].is_ascii_alphabetic() && drive.ends_with(':'),
            "Windows ドライブ名が不正"
        );
        (Url::parse(&format!("file:///{drive}/"))?, rest)
    };
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Windows パスを URL にできない"))?
        .pop_if_empty()
        .extend(rest.split('/'));
    Ok(url)
}

#[cfg(test)]
mod tests;
