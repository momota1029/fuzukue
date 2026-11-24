use super::*;
use crate::mail::{ImapSource, MailSource, REVIEW_INVITE};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const HEADERS: &str = "Date: Sat, 1 Jun 2024 09:00:00 +0900\r\nFrom: editor@example.invalid\r\nSubject: =?ISO-2022-JP?B?GyRCRnxLXBsoQg==?=\r\nMessage-ID: <1@example.invalid>\r\n\r\n";
const REVIEW: &str =
    "Subject: Invitation to review\r\nMessage-ID: <review@example.invalid>\r\n\r\n";

fn fetch(uid: u32, section: &str, bytes: &str, extras: &str) -> String {
    format!(
        "* 1 FETCH (UID {uid} {section} {{{}}}\r\n{bytes}{extras})\r\n",
        bytes.len()
    )
}

fn fixture(
    steps: Vec<(&str, String)>,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let steps = steps
        .into_iter()
        .map(|(command, reply)| (command.to_owned(), reply))
        .collect::<Vec<_>>();
    let (client, server) = tokio::io::duplex(16 * 1024);
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        server
            .get_mut()
            .write_all(b"* OK fixture ready\r\n")
            .await
            .unwrap();
        let mut log = Vec::new();
        for (expected, response) in steps {
            let mut line = String::new();
            assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
            let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
            let mut command = command.to_owned();
            if let Some((_, literal)) = expected.split_once("\r\n") {
                server.get_mut().write_all(b"+ continue\r\n").await.unwrap();
                let mut tail = vec![0; literal.len() + 2];
                server.read_exact(&mut tail).await.unwrap();
                assert!(tail.ends_with(b"\r\n"));
                command.push_str("\r\n");
                command.push_str(std::str::from_utf8(&tail[..tail.len() - 2]).unwrap());
            }
            assert_eq!(command, expected);
            log.push(command);
            server
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
            server
                .get_mut()
                .write_all(format!("{tag} OK done\r\n").as_bytes())
                .await
                .unwrap();
        }
        log
    });
    (client, task)
}

#[tokio::test]
async fn native_transport_fetches_headers_then_peeks_one_message() -> Result<()> {
    let folder = "研究 & 通知";
    let wire = encode_folder(folder)?;
    assert_eq!(decode_folder(&wire)?, folder);
    let exam = format!("EXAMINE \"{wire}\"");
    let exam = exam.as_str();
    let message = format!("{HEADERS}fixture body\r\n");
    let steps = vec![
        (
            "LOGIN \"fixture@example.invalid\" \"synthetic-password\"",
            String::new(),
        ),
        (
            "LIST \"\" \"*\"",
            format!("* LIST (\\Sent) \"/\" \"{wire}\"\r\n* LIST (\\Noselect) \"/\" \"skip\"\r\n"),
        ),
        (exam, "* OK [UIDVALIDITY 125] ids\r\n".into()),
        (
            "UID SEARCH FROM \"example.invalid\" UNSEEN",
            "* SEARCH 13 1234\r\n".into(),
        ),
        (
            "UID FETCH 1234 (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)])",
            fetch(
                1234,
                "BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)]",
                HEADERS,
                " FLAGS ()",
            ),
        ),
        (exam, "* OK [UIDVALIDITY 125] ids\r\n".into()),
        (
            "UID FETCH 1234 (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)])",
            fetch(
                1234,
                "BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)]",
                HEADERS,
                " FLAGS (\\Seen)",
            ),
        ),
        (
            "UID FETCH 1234 (UID BODY.PEEK[]<0.67108865>)",
            fetch(1234, "BODY[]<0>", &message, ""),
        ),
    ];
    let (stream, peer) = fixture(steps);
    let mut source = ImapSource(
        Transport::authenticate(
            stream,
            "fixture@example.invalid",
            Secret::Password("synthetic-password".into()),
        )
        .await?,
    );
    let hits = source
        .search(
            "sent",
            &[Filter::From("example.invalid".into()), Filter::Unseen],
            None,
            1,
        )
        .await?;
    assert_eq!(hits.folder, folder);
    assert_eq!(hits.headers[0].uid.get(), 1234);
    assert_eq!(
        crate::mail::mime::Headers::parse(&hits.headers[0].bytes)?.subject,
        "日本"
    );
    assert!(!hits.headers[0].seen);
    let review = regex::Regex::new(REVIEW_INVITE)?;
    let (raw, seen) = source
        .read(folder, Uid::new(1234).unwrap(), hits.validity, &review)
        .await?;
    assert_eq!(raw, message.as_bytes());
    assert!(seen);
    let log = peer.await?;
    assert!(log.iter().all(|line| !line
        .split_ascii_whitespace()
        .any(|word| ["SELECT", "STORE", "APPEND", "EXPUNGE", "COPY", "MOVE",].contains(&word))));
    Ok(())
}

#[tokio::test]
async fn review_refusal_stops_before_body_fetch_and_checks_validity() -> Result<()> {
    let steps = vec![
        (
            "LOGIN \"fixture@example.invalid\" \"synthetic-password\"",
            String::new(),
        ),
        ("EXAMINE \"INBOX\"", "* OK [UIDVALIDITY 9] ids\r\n".into()),
        (
            "UID FETCH 42 (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)])",
            fetch(
                42,
                "BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)]",
                REVIEW,
                " FLAGS ()",
            ),
        ),
        ("EXAMINE \"INBOX\"", "* OK [UIDVALIDITY 10] ids\r\n".into()),
    ];
    let (stream, peer) = fixture(steps);
    let mut source = ImapSource(
        Transport::authenticate(
            stream,
            "fixture@example.invalid",
            Secret::Password("synthetic-password".into()),
        )
        .await?,
    );
    let review = regex::RegexBuilder::new(REVIEW_INVITE)
        .case_insensitive(true)
        .build()?;
    let uid = Uid::new(42).unwrap();
    assert!(source
        .read("INBOX", uid, 9, &review)
        .await
        .unwrap_err()
        .to_string()
        .contains("査読依頼"));
    assert!(source
        .read("INBOX", uid, 9, &review)
        .await
        .unwrap_err()
        .to_string()
        .contains("検索し直す"));
    assert_eq!(peer.await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn xoauth2_uses_sasl_challenge_and_gmail_raw_is_a_single_operand() -> Result<()> {
    use base64::engine::general_purpose::STANDARD;
    let (stream, peer) = tokio::io::duplex(2048);
    let peer = tokio::spawn(async move {
        let mut peer = BufReader::new(peer);
        peer.get_mut()
            .write_all(b"* OK fixture ready\r\n")
            .await
            .unwrap();
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let (tag, cmd) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
        assert_eq!(cmd, "AUTHENTICATE XOAUTH2");
        let tag = tag.to_owned();
        peer.get_mut().write_all(b"+ \r\n").await.unwrap();
        line.clear();
        peer.read_line(&mut line).await.unwrap();
        let bytes = STANDARD.decode(line.trim()).unwrap();
        assert_eq!(
            bytes,
            b"user=fixture@example.invalid\x01auth=Bearer synthetic-access\x01\x01"
        );
        peer.get_mut()
            .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
            .await
            .unwrap();
    });
    Transport::authenticate(
        stream,
        "fixture@example.invalid",
        Secret::OAuth2("synthetic-access".into()),
    )
    .await?;
    peer.await?;

    assert_eq!(
        search_query(&[Filter::GmailRaw("from:test subject:\"review\"".into())],)?,
        "CHARSET UTF-8 X-GM-RAW \"from:test subject:\\\"review\\\"\""
    );
    assert_eq!(
        search_query(&[Filter::Text("研究".into())])?,
        "CHARSET UTF-8 TEXT {6}\r\n研究"
    );
    assert!(search_query(&[Filter::Text("foo\r\nAPPEND".into())]).is_err());
    assert_eq!(encode_folder("受信 & 😀")?, "&U9dP4Q- &- &2D3eAA-");
    Ok(())
}

#[tokio::test]
async fn utf8_literal_and_attachment_filter_do_not_fetch_bodies() -> Result<()> {
    let plain = fetch(
        8,
        "BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)]",
        HEADERS,
        r#" FLAGS (\Seen) BODYSTRUCTURE ("TEXT" "PLAIN" NIL NIL NIL "7BIT" 12 1 NIL NIL NIL NIL)"#,
    );
    let attachment = fetch(
        7,
        "BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)]",
        HEADERS,
        r#" FLAGS () BODYSTRUCTURE ("APPLICATION" "PDF" NIL NIL NIL "BASE64" 12 NIL ("ATTACHMENT" ("FILENAME" "paper.pdf")) NIL NIL)"#,
    );
    let steps = vec![
        ("LOGIN \"fixture@example.invalid\" \"synthetic-password\"", String::new()),
        ("EXAMINE \"INBOX\"", "* OK [UIDVALIDITY 9] ids\r\n".into()),
        ("UID SEARCH CHARSET UTF-8 SINCE 01-Oct-2024 BEFORE 01-Nov-2024 SUBJECT {6}\r\n研究",
            "* SEARCH 7 8\r\n".into()),
        ("UID FETCH 8,7 (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS)] BODYSTRUCTURE)",
            plain + &attachment),
    ];
    let (stream, peer) = fixture(steps);
    let mut source = ImapSource(
        Transport::authenticate(
            stream,
            "fixture@example.invalid",
            Secret::Password("synthetic-password".into()),
        )
        .await?,
    );
    let filters = [
        Filter::Since(chrono::NaiveDate::from_ymd_opt(2024, 10, 1).unwrap()),
        Filter::Before(chrono::NaiveDate::from_ymd_opt(2024, 11, 1).unwrap()),
        Filter::Subject("研究".into()),
    ];
    let rows = source.search("INBOX", &filters, Some(true), 1).await?;
    assert_eq!(rows.headers.len(), 1);
    assert_eq!(rows.headers[0].uid.get(), 7);
    assert_eq!(rows.headers[0].has_attachment, Some(true));
    assert_eq!(peer.await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn thread_walks_across_folders_and_leaves_review_body_on_server() -> Result<()> {
    use crate::mail::{unwrap_mail, wrap};

    const FIELDS: &str =
        "BODY[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES AUTHENTICATION-RESULTS)]";
    const ROOT: &str = "Date: Wed, 2 Oct 2024 12:00:00 +0000\r\nSubject: First\r\nMessage-ID: <root@example.invalid>\r\n\r\n";
    const SENT: &str = "Date: Thu, 3 Oct 2024 12:00:00 +0000\r\nSubject: Re: First\r\nMessage-ID: <sent@example.invalid>\r\nReferences: <root@example.invalid>\r\n\r\n";
    const REVIEW_CHILD: &str = "Date: Fri, 4 Oct 2024 12:00:00 +0000\r\nSubject: Invitation to review\r\nMessage-ID: <child@example.invalid>\r\nIn-Reply-To: <sent@example.invalid>\r\nAuthentication-Results: mx.example.invalid; dmarc=fail\r\n\r\n";
    let root_query = r#"UID SEARCH OR OR HEADER "Message-ID" "<root@example.invalid>" HEADER "References" "<root@example.invalid>" HEADER "In-Reply-To" "<root@example.invalid>""#;
    let sent_query = root_query.replace("<root@", "<sent@");
    let child_query = root_query.replace("<root@", "<child@");
    let check_inbox = (
        "EXAMINE \"INBOX\"",
        "* OK [UIDVALIDITY 10] ids\r\n".to_owned(),
    );
    let check_sent = (
        "EXAMINE \"Sent Items\"",
        "* OK [UIDVALIDITY 20] ids\r\n".to_owned(),
    );
    let get_root = "UID FETCH 7 (UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES AUTHENTICATION-RESULTS)])";
    let get_sent = get_root.replace("FETCH 7", "FETCH 9");
    let get_child = get_root.replace("FETCH 7", "FETCH 11");
    let steps = vec![
        (
            "LOGIN \"fixture@example.invalid\" \"synthetic-password\"",
            String::new(),
        ),
        check_inbox.clone(),
        (get_root, fetch(7, FIELDS, ROOT, " FLAGS (\\Seen)")),
        (
            "LIST \"\" \"*\"",
            "* LIST () \"/\" \"INBOX\"\r\n* LIST (\\Sent) \"/\" \"Sent Items\"\r\n".into(),
        ),
        check_inbox.clone(),
        (root_query, "* SEARCH 7\r\n".into()),
        check_sent.clone(),
        (root_query, "* SEARCH 9\r\n".into()),
        (&get_sent, fetch(9, FIELDS, SENT, " FLAGS (\\Seen)")),
        check_inbox.clone(),
        (&sent_query, "* SEARCH 11\r\n".into()),
        (&get_child, fetch(11, FIELDS, REVIEW_CHILD, " FLAGS ()")),
        check_sent.clone(),
        (&sent_query, "* SEARCH 9\r\n".into()),
        check_inbox.clone(),
        (&child_query, "* SEARCH 11\r\n".into()),
        check_sent.clone(),
        (&child_query, "* SEARCH\r\n".into()),
        check_inbox,
        (
            "UID FETCH 7 (UID BODY.PEEK[]<0.67108865>)",
            fetch(7, "BODY[]<0>", &format!("{ROOT}First body\r\n"), ""),
        ),
        check_sent,
        (
            "UID FETCH 9 (UID BODY.PEEK[]<0.67108865>)",
            fetch(9, "BODY[]<0>", &format!("{SENT}Sent body\r\n"), ""),
        ),
    ];
    let (stream, peer) = fixture(steps);
    let mut source = ImapSource(
        Transport::authenticate(
            stream,
            "fixture@example.invalid",
            Secret::Password("synthetic-password".into()),
        )
        .await?,
    );
    let review = regex::RegexBuilder::new(REVIEW_INVITE)
        .case_insensitive(true)
        .build()?;
    let thread = source
        .thread("fixture", "INBOX", Uid::new(7).unwrap(), 10, &review)
        .await?;
    assert_eq!(thread.validities["Sent Items"], 20);
    assert_eq!(thread.data["truncated"], false);
    let messages = thread.data["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        unwrap_mail(messages[0]["body"].as_str().unwrap())?,
        "First body\n"
    );
    assert_eq!(
        unwrap_mail(messages[1]["body"].as_str().unwrap())?,
        "Sent body\n"
    );
    let sent_id = "fixture:Sent Items:9";
    assert_eq!(
        messages[1]["id"],
        wrap(&crate::mail::message_key(sent_id, 20), sent_id)
    );
    assert_eq!(
        unwrap_mail(messages[2]["subject"].as_str().unwrap())?,
        "Invitation to review"
    );
    assert_eq!(messages[2]["tags"], serde_json::json!(["未読", "査読"]));
    assert!(messages[2]["body"].is_null());
    assert!(messages[2]["attachments"].is_null());
    assert_eq!(messages[2]["body_withheld"], true);
    assert_eq!(messages[2]["sender_warning"], "送信者を確認できない");
    let log = peer.await?;
    assert_eq!(
        log.iter()
            .filter(|line| line.contains("BODY.PEEK[]"))
            .count(),
        2
    );
    Ok(())
}
