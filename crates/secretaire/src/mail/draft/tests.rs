use super::*;
use crate::mail::{mime, unwrap_mail};
use mailparse::MailHeaderMap;

fn args() -> Args {
    serde_json::from_value(json!({
        "account": "fixture",
        "to": "Person <recipient@example.invalid>",
        "cc": "cc@example.invalid",
        "subject": "研究's note,attachment=/wrong",
        "body": "本文\nSecond, line",
        "attach": ["研究.csv"],
    }))
    .unwrap()
}

#[test]
fn reply_draft_has_encoded_subject_thread_headers_and_attachment() -> Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("研究.csv"), b"fixture")?;
    let reply = reply(&parent())?;
    let result = write(
        project.path(),
        "sender@example.invalid",
        args(),
        Some(reply),
        None,
    )?;
    let path = result["path"].as_str().unwrap();
    assert!(path.starts_with("outbox/"));
    assert!(path.ends_with(".eml"));
    assert_eq!(result["compose_started"], false);
    let bytes = std::fs::read(project.path().join(path))?;
    let mail = mailparse::parse_mail(&bytes)?;
    assert!(String::from_utf8_lossy(&bytes).contains("Subject: =?UTF-8?B?"));
    assert_eq!(
        mail.headers.get_first_value("Subject").as_deref(),
        Some("研究's note,attachment=/wrong")
    );
    assert_eq!(
        mail.headers.get_first_value("X-Unsent").as_deref(),
        Some("1")
    );
    assert_eq!(
        mail.headers.get_first_value("In-Reply-To").as_deref(),
        Some("<parent@example.invalid>")
    );
    assert_eq!(
        mail.headers.get_first_value("References").as_deref(),
        Some("<first@example.invalid> <middle@example.invalid> <parent@example.invalid>")
    );
    assert_eq!(
        mail.subparts[0].get_body()?.trim_end(),
        "本文\r\nSecond, line"
    );
    assert_eq!(
        mime::attachment(&bytes, 3)?,
        ("研究.csv".into(), b"fixture".to_vec())
    );
    Ok(())
}

#[test]
fn save_attachment_stays_in_project_and_never_overwrites() -> Result<()> {
    let project = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let raw = "Date: Tue, 1 Oct 2024 10:00:00 +0000\r\nSubject: ../研究\r\nContent-Type: multipart/mixed; boundary=BOUNDARY\r\n\r\n--BOUNDARY\r\nContent-Type: text/plain\r\n\r\nfixture body\r\n--BOUNDARY\r\nContent-Disposition: attachment; filename=\"../../paper.pdf\"\r\n\r\ndocument\r\n--BOUNDARY--\r\n".as_bytes();
    let (name, data) = mime::attachment(raw, 3)?;
    let headers = Headers::parse(raw)?;
    let path = save_attachment(project.path(), None, &headers, &name, &data)?;
    assert_eq!(path, Path::new("inbox/2024-10-01-研究/paper.pdf"));
    let second = save_attachment(project.path(), None, &headers, &name, b"other")?;
    assert_eq!(second, Path::new("inbox/2024-10-01-研究/paper-2.pdf"));
    assert_eq!(std::fs::read(project.path().join(path))?, data);
    assert!(save_attachment(project.path(), Some("../outside"), &headers, &name, &data).is_err());
    std::os::unix::fs::symlink(outside.path(), project.path().join("outbox"))?;
    let mut no_attachment = args();
    no_attachment.attach.clear();
    assert!(write(
        project.path(),
        "sender@example.invalid",
        no_attachment,
        None,
        None
    )
    .is_err());
    assert!(std::fs::read_dir(outside.path())?.next().is_none());
    let value = crate::mail::mime::read(
        &crate::mail::RawMessage {
            id: "sample:INBOX:1".into(),
            key: "0123456789abcdef".into(),
            bytes: raw.to_vec(),
            seen: true,
        },
        100,
    )?;
    assert_eq!(value["attachments"][0]["part"], 3);
    assert!(unwrap_mail(value["body"].as_str().unwrap())?.contains("fixture body"));
    Ok(())
}

#[test]
fn compose_has_one_argument_and_uses_mocked_wsl_path_conversion() -> Result<()> {
    let mut args = args();
    let mut unthreadable = parent();
    unthreadable.bytes = b"Subject: Old\r\n\r\nPrior body".to_vec();
    let reply = reply(&unthreadable)?;
    args.body.push_str(reply.quote.as_deref().unwrap());
    let launcher = Launcher {
        executable: Some("/fake/thunderbird.exe".into()),
    };
    let command = launcher.command(
        Platform::Wsl,
        "sender@example.invalid",
        &args,
        &[DraftFile {
            file: "/home/fixture/研究.csv".into(),
            filename: "研究.csv".into(),
            data: Vec::new(),
        }],
        None,
        |path| {
            assert_eq!(path, Path::new("/home/fixture/研究.csv"));
            Ok(r"C:\mail folder\a,b's.csv".into())
        },
    )?;
    assert_eq!(command.get_program(), "/fake/thunderbird.exe");
    let argv = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(argv.len(), 2);
    assert_eq!(argv[0], "-compose");
    assert!(argv[1].contains("%27s%20note%2Cattachment%3D%2Fwrong"));
    assert!(argv[1].contains("body=%E6%9C%AC"), "{}", argv[1]);
    assert!(argv[1].contains("attachment='file:///C:/mail%20folder/a%2Cb%27s.csv'"));
    assert_eq!(argv[1].matches("attachment=").count(), 1);
    assert!(argv[1].contains("%3E%20Prior%20body"));
    assert!(!argv[1].contains("in-reply-to"));
    Ok(())
}

fn parent() -> RawMessage {
    RawMessage {
        id: "fixture:INBOX:1".into(),
        key: "0123456789abcdef".into(),
        bytes: b"From: fixture@example.invalid\r\nSubject: Old\r\nMessage-ID: <parent@example.invalid>\r\nReferences: <first@example.invalid>\r\n <middle@example.invalid>\r\n\r\nPrior body\r\n".to_vec(),
        seen: true,
    }
}

#[test]
fn reply_opens_editable_eml_with_mocked_wsl_path_conversion() -> Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(project.path().join("研究.csv"), b"fixture")?;
    let result = write(
        project.path(),
        "sender@example.invalid",
        args(),
        Some(reply(&parent())?),
        None,
    )?;
    let path = project.path().join(result["path"].as_str().unwrap());
    let launcher = Launcher {
        executable: Some("/fake/thunderbird.exe".into()),
    };
    let command = launcher.command(
        Platform::Wsl,
        "sender@example.invalid",
        &args(),
        &[],
        Some(&path),
        |draft| {
            assert_eq!(draft, path);
            Ok(r"C:\mail folder\reply draft.eml".into())
        },
    )?;
    assert_eq!(command.get_program(), "/fake/thunderbird.exe");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-file", r"C:\mail folder\reply draft.eml"]
    );
    Ok(())
}
