use super::*;

fn account(auth: &str) -> Account {
    toml::from_str(&format!(
        r#"
kind = "imap"
name = "fixture"
host = "imap.example.invalid"
user = "researcher@example.invalid"
auth = "{auth}"
client_id = "synthetic-public-client"
authority = "https://login.example.invalid/common/oauth2/v2.0/"
scope = "example.imap offline_access"
redirect_uri = "http://localhost:4321/redirect"
"#
    ))
    .unwrap()
}

#[test]
fn redirect_checks_state_and_pkce_hash() -> Result<()> {
    let flow = Flow::new(&account("oauth2"))?;
    let params = flow.url.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(flow.url.path(), "/common/oauth2/v2.0/authorize");
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()))
    );
    assert!(flow.verifier.len() >= 43);
    let url = format!("{}?code=fixture%2Bcode&state={}", flow.redirect, flow.state);
    assert_eq!(flow.code(&url)?, "fixture+code");
    assert!(flow.code(&url.replace("/redirect", "/another")).is_err());
    assert!(flow.code(&url.replace(&flow.state, "wrong")).is_err());
    assert!(flow.code(&format!("{url}&code=duplicate")).is_err());
    Ok(())
}

#[test]
fn import_private_storage_and_refresh_request() -> Result<()> {
    use std::cell::Cell;

    struct Mock(Cell<usize>);
    impl TokenHttp for Mock {
        fn post(&self, url: &Url, form: &[(&str, &str)]) -> Result<TokenResponse> {
            self.0.set(self.0.get() + 1);
            assert_eq!(
                url.as_str(),
                "https://login.example.invalid/common/oauth2/v2.0/token"
            );
            assert_eq!(
                form,
                [
                    ("client_id", "synthetic-public-client"),
                    ("scope", "example.imap offline_access"),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", "synthetic-refresh"),
                ]
            );
            Ok(TokenResponse {
                access_token: "synthetic-access".into(),
                refresh_token: Some("rotated-synthetic-refresh".into()),
                expires_in: 3600,
            })
        }
    }

    let temp = tempfile::tempdir()?;
    let source = temp.path().join("synthetic-import.json");
    let legacy = br#"{"fixture":{"user":"researcher@example.invalid","refresh_token":"synthetic-refresh","access_token":"ignore-this"},"password":{"user":"researcher@example.invalid","app_password":"synthetic-password"}}"#;
    fs::write(&source, legacy)?;
    let path = temp.path().join("config").join("mail-credentials.json");
    let file = CredentialFile::new(path.clone());
    let oauth = account("oauth2");
    let mut password = account("password");
    password.name = "password".into();
    let accounts = BTreeMap::from([
        (oauth.name.clone(), oauth),
        (password.name.clone(), password),
    ]);
    assert_eq!(file.import(&source, &accounts)?, 2);
    assert_eq!(fs::read(source)?, legacy);
    assert_eq!(path.metadata()?.mode() & 0o777, 0o600);
    assert_eq!(path.parent().unwrap().metadata()?.mode() & 0o777, 0o700);
    assert!(!fs::read_to_string(&path)?.contains("ignore-this"));

    let mut tokens = Tokens::default();
    let mock = Mock(Cell::new(0));
    for _ in 0..2 {
        assert!(
            matches!(file.secret_with(&accounts["fixture"], &mut tokens, &mock)?,
            Secret::OAuth2(token) if token == "synthetic-access")
        );
    }
    assert_eq!(mock.0.get(), 1);
    assert!(
        matches!(file.secret_with(&accounts["password"], &mut tokens, &mock)?,
        Secret::Password(password) if password == "synthetic-password")
    );
    let persisted = fs::read_to_string(path)?;
    assert!(persisted.contains("rotated-synthetic-refresh"));
    assert!(!persisted.contains("synthetic-access"));
    Ok(())
}

#[test]
fn explicit_endpoints_secret_and_extra_auth_params() -> Result<()> {
    use std::cell::Cell;

    let account: Account = toml::from_str(
        r#"
kind = "imap"
name = "google-like"
host = "imap.example.invalid"
user = "researcher@example.invalid"
auth = "oauth2"
client_id = "synthetic-desktop-client"
client_secret = "synthetic-secret"
authorize_url = "https://accounts.example.invalid/o/oauth2/v2/auth"
token_url = "https://oauth2.example.invalid/token"
scope = "https://mail.example.invalid/"
redirect_uri = "http://localhost"
auth_params = { access_type = "offline", prompt = "consent" }
"#,
    )?;
    let flow = Flow::new(&account)?;
    assert_eq!(flow.url.host_str(), Some("accounts.example.invalid"));
    assert_eq!(flow.url.path(), "/o/oauth2/v2/auth");
    let params = flow.url.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(params["access_type"], "offline");
    assert_eq!(params["prompt"], "consent");

    struct Mock(Cell<bool>);
    impl TokenHttp for Mock {
        fn post(&self, url: &Url, form: &[(&str, &str)]) -> Result<TokenResponse> {
            self.0.set(true);
            assert_eq!(url.as_str(), "https://oauth2.example.invalid/token");
            assert!(form.contains(&("client_secret", "synthetic-secret")));
            Ok(TokenResponse { access_token: "a".into(), refresh_token: None, expires_in: 60 })
        }
    }
    let mock = Mock(Cell::new(false));
    exchange(&account, &mock, &[("grant_type", "refresh_token"), ("refresh_token", "r")])?;
    assert!(mock.0.get());
    Ok(())
}
