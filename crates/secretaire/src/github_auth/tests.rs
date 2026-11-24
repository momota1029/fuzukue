use std::{collections::VecDeque, sync::Mutex as StdMutex};

use super::*;

struct Fake {
    responses: StdMutex<VecDeque<Value>>,
    requests: StdMutex<Vec<(Instant, Request)>>,
}

impl Fake {
    fn new(responses: impl IntoIterator<Item = Value>) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().collect()),
            requests: StdMutex::default(),
        }
    }
}

impl Http for Fake {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Value>> {
        self.requests
            .lock()
            .unwrap()
            .push((Instant::now(), request));
        Box::pin(async {
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected HTTP request"))
        })
    }
}

const TEST_DEVICE: &str = "fake-device-secret";
const TEST_TOKEN: &str = "fake-access-secret";

fn device(life: u64) -> Value {
    json!({
        "device_code": TEST_DEVICE,
        "user_code": "ABCD-EFGH",
        "verification_uri": VERIFY,
        "expires_in": life,
        "interval": 5,
    })
}

fn fake_credential() -> Credential {
    Credential {
        client_id: "example".into(),
        scope: Scope::PublicRepo,
        token: TEST_TOKEN.into(),
        login: "reporter".into(),
        avatar_url: None,
    }
}

#[tokio::test(start_paused = true)]
async fn polls_pending_and_slow_down_then_resolves_the_account() {
    let fake = Fake::new([
        device(120),
        json!({"error":"authorization_pending"}),
        json!({"error":"slow_down","interval":10}),
        json!({"error":"authorization_pending"}),
        json!({"access_token":TEST_TOKEN}),
        json!({"login":"reporter"}),
    ]);
    let started = Instant::now();
    let device = Device::start(&fake, "example", Scope::PublicRepo)
        .await
        .unwrap();
    let serialized =
        serde_json::to_string(&event(1, Ok(device_update(&device.state()).unwrap()))).unwrap();
    assert!(serialized.contains("ABCD-EFGH"));
    assert!(!serialized.contains(TEST_DEVICE));
    let result = device
        .authorize(&fake, "example", Scope::PublicRepo)
        .await
        .unwrap();
    assert_eq!(result.login, "reporter");
    assert_eq!(result.token, TEST_TOKEN);

    let requests = fake.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|(time, _)| (*time - started).as_secs())
            .collect::<Vec<_>>(),
        [0, 5, 10, 20, 30, 30],
    );
    assert_eq!(requests[0].1.url, DEVICE_CODE);
    let Body::Form(body) = &requests[0].1.body else {
        panic!("expected device form")
    };
    assert_eq!(body, "client_id=example&scope=public_repo");
    for (_, request) in &requests[1..5] {
        assert_eq!(request.url, ACCESS_TOKEN);
        assert!(request.token.is_none());
        let Body::Form(body) = &request.body else {
            panic!("expected access token form")
        };
        let fields: std::collections::BTreeMap<_, _> =
            url::form_urlencoded::parse(body.as_bytes()).collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields["device_code"], TEST_DEVICE);
        assert_eq!(
            fields["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(fields["client_id"], "example");
    }
    assert_eq!(requests[5].1.url, "https://api.github.com/user");
    assert_eq!(requests[5].1.token.as_deref(), Some(TEST_TOKEN));
}

#[tokio::test(start_paused = true)]
async fn stops_after_denial_or_expiry_and_redacts_error_details() {
    for (remote, expected) in [
        ("access_denied", Error::Denied),
        ("expired_token", Error::Expired),
        ("unknown", Error::Response),
    ] {
        let fake = Fake::new([
            device(120),
            json!({"error":remote, "error_description":TEST_TOKEN}),
        ]);
        let device = Device::start(&fake, "example", Scope::Repo).await.unwrap();
        let error = device
            .authorize(&fake, "example", Scope::Repo)
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert!(!format!("{error:?}").contains(TEST_TOKEN));
        assert!(!serde_json::to_string(&event(1, Err(error)))
            .unwrap()
            .contains(TEST_TOKEN));
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let Body::Form(body) = &requests[0].1.body else {
            panic!("expected device form")
        };
        assert_eq!(body, "client_id=example&scope=repo");
    }

    let fake = Fake::new([device(6), json!({"error":"authorization_pending"})]);
    let device = Device::start(&fake, "example", Scope::PublicRepo)
        .await
        .unwrap();
    let error = device
        .authorize(&fake, "example", Scope::PublicRepo)
        .await
        .err();
    assert_eq!(error, Some(Error::Expired));
    assert_eq!(fake.requests.lock().unwrap().len(), 2);
}

#[test]
fn stores_credentials_outside_the_workspace_and_removes_them_on_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    let file = TokenFile(tmp.path().join("state").join("github-oauth.json"));
    let path = fuzukue_proto::compat::workspace_config(&project);
    fs::write(path, "[github]\noauth_client_id = \"example\"\n").unwrap();
    let mut config = GitHubConfig::from_workspace(&project).unwrap();
    file.save(&fake_credential(), &project).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file.0).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(file.load(&config).unwrap().unwrap().token, TEST_TOKEN);
    config.oauth_scope = Scope::Repo;
    assert!(file.load(&config).unwrap().is_none());
    assert_eq!(
        TokenFile(project.join("token")).save(&fake_credential(), &project),
        Err(Error::Storage)
    );
    file.remove().unwrap();
    file.remove().unwrap();
    assert!(!file.0.exists());
}

#[tokio::test]
async fn issue_post_uses_the_approved_content_and_only_requested_labels() {
    let fake = Fake::new([json!({"number":42}), json!({"number":43})]);
    let credential = fake_credential();
    for (label, number) in [(None, 42), (Some("from-fuzukue"), 43)] {
        assert_eq!(
            create_issue(
                &fake,
                &credential,
                "owner/project",
                "題 & name",
                "line\n",
                label
            )
            .await
            .unwrap(),
            format!("https://github.com/owner/project/issues/{number}"),
        );
    }
    let requests = fake.requests.lock().unwrap();
    let expected = [
        json!({"title":"題 & name","body":"line\n"}),
        json!({"title":"題 & name","body":"line\n","labels":["from-fuzukue"]}),
    ];
    for ((_, request), expected) in requests.iter().zip(expected) {
        assert_eq!(
            request.url,
            "https://api.github.com/repos/owner/project/issues"
        );
        assert_eq!(request.token.as_deref(), Some(TEST_TOKEN));
        let Body::Json(body) = &request.body else {
            panic!("expected JSON POST")
        };
        assert_eq!(body, &expected);
    }
}

#[test]
fn report_destination_is_constrained_by_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    let config = fuzukue_proto::compat::workspace_config(tmp.path());
    for (settings, origin, expected) in [
        ("[github]", Some("Owner/Origin"), Some("owner/origin")),
        ("[github]", None, None),
        (
            "[github]\nreport_repo = 'Owner/Reports'",
            Some("Owner/Origin"),
            Some("owner/reports"),
        ),
        (
            "[github]\nreport_repo = 'Owner/Reports'\nissue_repos = ['owner/origin']",
            Some("Owner/Origin"),
            None,
        ),
        (
            "[github]\nreport_repo = 'Owner/Reports'\nissue_repos = ['OWNER/REPORTS']",
            None,
            Some("owner/reports"),
        ),
        (
            "[github]\nreport_repo = 'owner/../reports'",
            Some("Owner/Origin"),
            None,
        ),
    ] {
        fs::write(&config, settings).unwrap();
        assert_eq!(
            GitHubConfig::from_workspace(tmp.path())
                .unwrap()
                .report_target(origin)
                .as_deref(),
            expected,
        );
    }
}

fn report_workspace(tmp: &tempfile::TempDir, fake: Fake) -> Arc<Workspace> {
    let project = tmp.path().join("project");
    let state = tmp.path().join("state");
    fs::create_dir(&project).unwrap();
    fs::write(
        project.join("fuzukue.toml"),
        "[github]\noauth_client_id = 'example'\nreport_repo = 'owner/report'\n",
    )
    .unwrap();
    let mut app = Workspace::new(
        project,
        "127.0.0.1:1".into(),
        state.clone(),
        Arc::new(crate::Hub::with_timeline(&state)),
    );
    app.github_auth.http = Box::new(fake);
    Arc::new(app)
}

#[tokio::test]
async fn submit_validates_the_repository_and_the_previewed_account() {
    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(&tmp, Fake::new([json!({"number":7})]));
    app.github_auth
        .token
        .save(&fake_credential(), &app.project_dir)
        .unwrap();
    let (reply, _) = mpsc::unbounded_channel();
    for (request, (repo, login, title, expected)) in [
        ("owner/other", "reporter", "Bug", Err(Error::Repository)),
        (
            "owner/report",
            "another-user",
            "Bug",
            Err(Error::AccountChanged),
        ),
        ("owner/report", "reporter", " ", Err(Error::EmptyReport)),
        (
            "OWNER/REPORT",
            "reporter",
            "Bug",
            Ok(Update::Submitted {
                url: "https://github.com/owner/report/issues/7".into(),
            }),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            handle(
                &app,
                request as u64,
                Action::Submit {
                    repo: repo.into(),
                    login: login.into(),
                    title: title.into(),
                    body: "Reproduction steps".into(),
                },
                &reply
            )
            .await,
            expected
        );
    }
}

#[tokio::test]
async fn changed_oauth_settings_disconnect_the_stored_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(&tmp, Fake::new([]));
    app.github_auth
        .token
        .save(&fake_credential(), &app.project_dir)
        .unwrap();
    let (reply, _events) = mpsc::unbounded_channel();
    let status = |login| Update::Status {
        status: Status {
            enabled: true,
            login,
            report_repo: Some("owner/report".into()),
            workspace_kind: workspace_kind(&app.project_dir),
        },
    };
    assert_eq!(
        handle(&app, 0, Action::Status, &reply).await,
        Ok(status(Some("reporter".into())))
    );

    for (client_id, scope) in [("different", "public_repo"), ("example", "repo")] {
        fs::write(
            app.project_dir.join("fuzukue.toml"),
            format!(
                "[github]\noauth_client_id = '{client_id}'\noauth_scope = '{scope}'\nreport_repo = 'owner/report'\n"
            ),
        )
        .unwrap();
        assert_eq!(
            handle(&app, 1, Action::Status, &reply).await,
            Ok(status(None))
        );
        assert_eq!(
            handle(
                &app,
                2,
                Action::Submit {
                    repo: "owner/report".into(),
                    login: "reporter".into(),
                    title: "Bug".into(),
                    body: "Reproduction steps".into(),
                },
                &reply,
            )
            .await,
            Err(Error::NotConnected)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn disconnect_cancels_an_active_device_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(&tmp, Fake::new([device(120)]));
    let (reply, mut events) = mpsc::unbounded_channel();
    let original = app.clone();
    let start = reply.clone();
    let connecting =
        tokio::spawn(async move { handle(&original, 1, Action::Connect, &start).await });
    assert!(matches!(
        events.recv().await,
        Some(ServerEvent::GitHubReportResult {
            request: 1,
            result: Ok(Update::Device { .. }),
        })
    ));
    assert!(matches!(
        handle(&app, 2, Action::Disconnect, &reply).await,
        Ok(Update::Status {
            status: Status { login: None, .. }
        })
    ));
    assert_eq!(connecting.await.unwrap(), Err(Error::Cancelled));
    assert!(!app.github_auth.token.0.exists());
}

#[tokio::test(start_paused = true)]
async fn chat_and_dialog_share_one_attempt_and_only_publish_public_data() {
    use fuzukue_proto::Actor;
    use mcp::Tool;

    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(
        &tmp,
        Fake::new([
            device(120),
            json!({"error":"authorization_pending"}),
            json!({"error":"slow_down"}),
            json!({"access_token":TEST_TOKEN}),
            json!({"login":"reporter","avatar_url":"https://avatars.githubusercontent.com/u/123?v=4"}),
        ]),
    );
    let (_, mut live) = app.hub.subscribe(0);
    app.hub.set_github_auth_request("chat", Some(Tool::Start));
    let (reply, mut dialog) = mpsc::unbounded_channel();
    let dialog_task = {
        let app = app.clone();
        tokio::spawn(async move { handle(&app, 1, Action::Connect, &reply).await })
    };
    let dialog_code = dialog.recv().await.unwrap();
    let start = Tool::Start.call(&app, "chat", Actor::System).await.unwrap();
    assert_eq!(start["state"], "pending");
    assert_eq!(start["code"], "ABCD-EFGH");
    let ServerEvent::GitHubReportResult {
        result:
            Ok(Update::Device {
                user_code,
                verification_uri,
                expires_at,
            }),
        ..
    } = &dialog_code
    else {
        panic!("expected a device code in the dialog")
    };
    assert_eq!(start["code"], user_code.as_str());
    assert_eq!(start["url"], verification_uri.as_str());
    assert_eq!(start["expires_at"], *expires_at);
    assert_eq!(
        Tool::Status
            .call(&app, "chat", Actor::System)
            .await
            .unwrap(),
        json!({"state":"pending"})
    );

    // The desk's tool output may also be recorded as a chat tool result.
    app.hub.publish(ServerEvent::ToolResult {
        session: "chat".into(),
        item: "connect".into(),
        ok: true,
        output: start.to_string(),
    });
    drop(dialog);
    assert_eq!(dialog_task.await.unwrap(), Err(Error::Cancelled));
    let mut cards = Vec::new();
    let mut serialized = vec![
        serde_json::to_string(&dialog_code).unwrap(),
        start.to_string(),
    ];
    loop {
        let envelope = live.recv().await.unwrap();
        serialized.push(serde_json::to_string(&envelope).unwrap());
        if let ServerEvent::GitHubConnectCard { attempt, state, .. } = envelope.event {
            let connected = matches!(state, State::Connected { .. });
            cards.push((attempt, state));
            if connected {
                break;
            }
        }
    }
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].0, cards[1].0);
    assert!(matches!(cards[0].1, State::Pending { .. }));
    assert_eq!(
        cards[1].1,
        State::Connected {
            login: "reporter".into()
        }
    );
    assert_eq!(
        Tool::Status
            .call(&app, "chat", Actor::System)
            .await
            .unwrap(),
        json!({"state":"connected","login":"reporter"})
    );
    serialized.push(serde_json::to_string(&Auth::snapshots(&app).await).unwrap());
    serialized.push(serde_json::to_string(&app.hub.subscribe(0).0).unwrap());
    let journal = fs::read_to_string(app.state_dir.join("thread-timeline.jsonl")).unwrap();
    assert!(journal.contains("ABCD-EFGH"));
    serialized.push(journal);
    for output in serialized {
        assert!(!output.contains(TEST_TOKEN));
        assert!(!output.contains(TEST_DEVICE));
        assert!(!output.contains("avatars.githubusercontent.com"));
    }
}

#[tokio::test(start_paused = true)]
async fn status_transitions_to_denied_and_a_retry_can_expire() {
    use fuzukue_proto::Actor;
    use mcp::Tool;
    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(
        &tmp,
        Fake::new([device(120), json!({"error":"access_denied"}), device(3)]),
    );
    app.hub.set_github_auth_request("chat", Some(Tool::Start));
    let (_, mut live) = app.hub.subscribe(0);
    for terminal in [State::Denied, State::Expired] {
        assert_eq!(
            Tool::Start.call(&app, "chat", Actor::System).await.unwrap()["state"],
            "pending"
        );
        loop {
            if let ServerEvent::GitHubConnectCard { state, .. } = live.recv().await.unwrap().event {
                if state == terminal {
                    break;
                }
            }
        }
        assert_eq!(
            Tool::Status
                .call(&app, "chat", Actor::System)
                .await
                .unwrap(),
            serde_json::to_value(terminal).unwrap()
        );
    }
}

#[tokio::test]
async fn mcp_refuses_automatic_turns_and_explains_missing_configuration() {
    use fuzukue_proto::{Actor, GitHubPolicy};
    use mcp::Tool;

    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(&tmp, Fake::new([]));
    for policy in [GitHubPolicy::Ask, GitHubPolicy::Allow] {
        app.issue_threads.test_start_automatic_turn("chat", policy);
        for tool in Tool::ALL {
            app.hub.set_github_auth_request("chat", Some(tool));
            let error = tool.call(&app, "chat", Actor::System).await.unwrap_err();
            assert!(error
                .to_string()
                .contains("automatic GitHub and issue-intake turns"));
        }
        app.issue_threads.finish("chat");
    }
    fs::write(app.project_dir.join("fuzukue.toml"), "[github]\n").unwrap();
    app.hub
        .set_github_auth_request("chat", mcp::request("GitHub につないで"));
    assert!(Tool::Start
        .call(&app, "chat", Actor::System)
        .await
        .unwrap_err()
        .to_string()
        .contains("docs/github-auth.md"));
    assert_eq!(
        Tool::Status
            .call(&app, "chat", Actor::System)
            .await
            .unwrap(),
        json!({"state":"not_configured"})
    );
    assert!(Tool::Start
        .call(&app, "other-thread", Actor::System)
        .await
        .unwrap_err()
        .to_string()
        .contains("direct request"));
    assert!(mcp::request("The issue says: connect GitHub").is_none());
    assert!(mcp::request("GitHub の接続を解除して").is_some());
}

#[tokio::test]
async fn mcp_disconnect_requires_a_human_approval() {
    use fuzukue_proto::Actor;
    use mcp::Tool;

    let tmp = tempfile::tempdir().unwrap();
    let app = report_workspace(&tmp, Fake::new([]));
    app.github_auth
        .token
        .save(&fake_credential(), &app.project_dir)
        .unwrap();
    app.hub
        .set_github_auth_request("chat", Some(Tool::Disconnect));
    let (_, mut live) = app.hub.subscribe(0);
    for decision in ["decline", "accept"] {
        let task = {
            let app = app.clone();
            tokio::spawn(async move { Tool::Disconnect.call(&app, "chat", Actor::System).await })
        };
        let request = loop {
            if let ServerEvent::ApprovalRequested { request_id, .. } =
                live.recv().await.unwrap().event
            {
                break request_id;
            }
        };
        assert!(app
            .delegates
            .resolve_approval(&request, "accept", true)
            .unwrap());
        assert!(app.github_auth.token.0.exists());
        assert!(app
            .delegates
            .resolve_approval(&request, decision, false)
            .unwrap());
        if decision == "accept" {
            assert_eq!(
                task.await.unwrap().unwrap(),
                json!({"state":"disconnected"})
            );
            assert!(!app.github_auth.token.0.exists());
        } else {
            assert!(task.await.unwrap().is_err());
            assert!(app.github_auth.token.0.exists());
        }
    }
}

#[test]
fn avatar_source_and_image_are_bounded() {
    use image::{DynamicImage, ImageFormat};

    assert_eq!(
        avatar::safe_url("https://avatars.githubusercontent.com/u/123?v=4").as_deref(),
        Some("https://avatars.githubusercontent.com/u/123?s=128")
    );
    for url in [
        "https://avatars.githubusercontent.com.attacker.invalid/u/123",
        "https://avatars.githubusercontent.com@attacker.invalid/u/123",
        "http://avatars.githubusercontent.com/u/123",
        "https://avatars.githubusercontent.com/u/123/extra",
        "https://avatars.githubusercontent.com:444/u/123",
    ] {
        assert!(avatar::safe_url(url).is_none());
    }
    for (format, mime) in [
        (ImageFormat::Jpeg, "image/jpeg"),
        (ImageFormat::Png, "image/png"),
    ] {
        let mut bytes = io::Cursor::new(Vec::new());
        DynamicImage::new_rgb8(512, 256)
            .write_to(&mut bytes, format)
            .unwrap();
        assert!(avatar::normalize("image/gif", bytes.get_ref()).is_none());
        assert!(avatar::normalize("image/webp", bytes.get_ref()).is_none());
        let png = avatar::normalize(mime, bytes.get_ref()).unwrap();
        let image = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
        assert_eq!((image.width(), image.height()), (128, 64));
        bytes.get_mut().resize(256 * 1024 + 1, 0);
        assert!(avatar::normalize(mime, bytes.get_ref()).is_none());
    }
    assert!(avatar::normalize("image/png", b"GIF87a").is_none());
}
