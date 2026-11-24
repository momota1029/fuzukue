use super::*;

#[test]
fn browser_prefill_encodes_and_preserves_the_preview() {
    let title = format!("{} & name", '\u{984c}');
    let link = browser_link("owner/project", &title, "first\nsecond #42").unwrap();
    assert_eq!(link.url.host_str(), Some("github.com"));
    assert_eq!(link.url.path(), "/owner/project/issues/new");
    assert_eq!(
        link.url.query_pairs().into_owned().collect::<Vec<_>>(),
        [
            ("title".into(), title),
            ("body".into(), "first\nsecond #42".into())
        ]
    );
    assert!(!link.omitted_title && !link.omitted_body);
    assert!(browser_link("owner/project?labels=from-fuzukue", "title", "body").is_none());
}

#[test]
fn browser_prefill_respects_the_encoded_url_limit() {
    let long = std::iter::repeat_n('\u{3042}', 1000).collect::<String>();
    let link = browser_link("owner/project", "short", &long).unwrap();
    assert_eq!(link.url.query_pairs().count(), 1);
    assert!(link.omitted_body && !link.omitted_title);
    let link = browser_link("owner/project", &long, &long).unwrap();
    assert!(link.url.query().is_none());
    assert!(link.omitted_title && link.omitted_body);
    assert!(link.url.as_str().len() <= BROWSER_URL_LIMIT);
}

#[test]
fn report_template_only_uses_public_diagnostics() {
    let body = template(GITHUB_REPORT_PROTOCOL, Some(WorkspaceKind::Research));
    for field in [
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        &GITHUB_REPORT_PROTOCOL.to_string(),
    ] {
        assert!(body.contains(field), "{body}");
    }
    assert!(!body.contains(env!("CARGO_MANIFEST_DIR")));
    assert!(!body.contains("github-report-template"));
}

#[test]
fn default_templates_fit_in_browser_prefill() {
    let mut args = fuzukue_i18n::FluentArgs::new();
    args.set("version", env!("CARGO_PKG_VERSION"));
    args.set("revision", "0".repeat(64));
    args.set("os", "windows");
    args.set("protocol", GITHUB_REPORT_PROTOCOL.to_string());
    for locale in fuzukue_i18n::Locale::available() {
        args.set("kind", locale.text("github-report-kind-research"));
        let title = locale.text("github-report-title-prefix");
        let body = locale.format("github-report-template", &args);
        let link = browser_link("example-owner/fuzukue", title, &body).unwrap();
        assert!(!link.omitted_body, "{}", locale.tag());
        assert!(body.lines().any(|line| line.starts_with("## ")));
    }
}
