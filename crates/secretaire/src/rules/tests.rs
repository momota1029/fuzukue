use super::*;
use crate::test_support::TestDir;

struct Repo {
    dir: TestDir,
    bare: PathBuf,
}

impl Repo {
    fn new() -> Result<Self> {
        let dir = TestDir::new();
        let bare = dir.path().join("source.git");
        fs::create_dir(&bare)?;
        git(Some(&bare), &["init", "--bare", "--quiet"])?;
        fs::create_dir(dir.path().join("work"))?;
        git(
            Some(&dir.path().join("work")),
            &["init", "--quiet", "-b", "main"],
        )?;
        Ok(Self { dir, bare })
    }

    fn source(&self) -> String {
        format!("file://{}", self.bare.display())
    }

    fn commit(&self) -> Result<String> {
        let work = self.dir.path().join("work");
        git(Some(&work), &["add", "--all"])?;
        git(
            Some(&work),
            &[
                "-c",
                "user.name=Rule Test",
                "-c",
                "user.email=rules@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--quiet",
                "-m",
                "rules",
            ],
        )?;
        git(Some(&work), &["push", "--quiet", &self.source(), "main"])?;
        git(Some(&work), &["rev-parse", "HEAD"])
    }

    fn tag(&self, tag: &str) -> Result<()> {
        let work = self.dir.path().join("work");
        git(
            Some(&work),
            &[
                "-c",
                "user.name=Rule Test",
                "-c",
                "user.email=rules@example.invalid",
                "-c",
                "tag.gpgSign=false",
                "tag",
                "-a",
                tag,
                "-m",
                "release",
            ],
        )?;
        git(Some(&work), &["push", "--quiet", &self.source(), tag])?;
        Ok(())
    }

    fn configure(&self, project: &TestDir, rev: &str) {
        project.write(
            "fuzukue.toml",
            &format!("[[rules]]\nsource = {:?}\nrev = {rev:?}\n", self.source()),
        );
    }
}

#[test]
fn pins_offline_and_updates_only_on_request() -> Result<()> {
    let project = TestDir::new();
    let repo = Repo::new()?;
    let cache = project.path().join("cache");
    let cache_dir = || Ok(cache.clone());
    repo.configure(&project, "main");
    repo.dir
        .write("work/reviewers/style.md", "old instructions");
    repo.dir
        .write("work/lint/removed.toml", "ignore_terms = []");
    let first = repo.commit()?;

    let old = resolve_with_cache(project.path(), cache_dir)?.0.remove(0);
    assert_eq!(old, cache_path(&cache, &repo.source(), &first)?);
    assert_eq!(Lock::read(project.path())?.rules[0].commit, first);
    let original_lock = fs::read(project.path().join("fuzukue.lock"))?;

    repo.dir
        .write("work/reviewers/style.md", "new instructions");
    repo.dir.write("work/policy/new.md", "new policy");
    fs::remove_file(repo.dir.path().join("work/lint/removed.toml"))?;
    let second = repo.commit()?;
    assert_eq!(
        resolve_with_cache(project.path(), cache_dir)?.0,
        [old.clone()]
    );
    let statuses = status(project.path())?;
    assert_eq!(statuses[0].locked_commit.as_deref(), Some(first.as_str()));
    assert_eq!(statuses[0].latest_commit.as_deref(), Some(second.as_str()));
    assert_eq!(statuses[0].newer, Some(true));
    assert_eq!(fs::read(project.path().join("fuzukue.lock"))?, original_lock);

    let offline = repo.bare.with_extension("offline");
    fs::rename(&repo.bare, &offline)?;
    assert_eq!(
        resolve_with_cache(project.path(), cache_dir)?.0,
        [old.clone()]
    );
    assert!(status(project.path())?[0].error.is_some());
    assert!(update_with_cache(project.path(), None, cache_dir).is_err());
    assert_eq!(fs::read(project.path().join("fuzukue.lock"))?, original_lock);
    fs::rename(&offline, &repo.bare)?;

    assert!(update_with_cache(project.path(), Some("absent"), cache_dir).is_err());
    let updated = update_with_cache(project.path(), Some(&repo.source()), cache_dir)?.remove(0);
    assert_eq!(updated.previous_commit.as_deref(), Some(first.as_str()));
    assert_eq!(updated.commit, second);
    let changes: Vec<_> = updated
        .changed_files
        .unwrap()
        .into_iter()
        .map(|change| (change.path, change.status))
        .collect();
    assert_eq!(
        changes,
        [
            ("lint/removed.toml".into(), "deleted"),
            ("policy/new.md".into(), "added"),
            ("reviewers/style.md".into(), "modified"),
        ]
    );
    let new = resolve_with_cache(project.path(), cache_dir)?.0.remove(0);
    assert_eq!(
        fs::read_to_string(new.join("reviewers/style.md"))?,
        "new instructions"
    );
    assert_eq!(
        fs::read_to_string(old.join("reviewers/style.md"))?,
        "old instructions"
    );
    assert_eq!(status(project.path())?[0].newer, Some(false));
    Ok(())
}

#[test]
fn pins_annotated_tags_and_raw_commits() -> Result<()> {
    let project = TestDir::new();
    let repo = Repo::new()?;
    repo.dir.write("work/policy/note.md", "pinned");
    let commit = repo.commit()?;
    repo.tag("v0.1.0")?;
    let cache = project.path().join("cache");
    for rev in ["v0.1.0", &commit] {
        repo.configure(&project, rev);
        let rules = resolve_with_cache(project.path(), || Ok(cache.clone()))?;
        let lock = Lock::read(project.path())?;
        assert!(lock
            .rules
            .iter()
            .any(|pin| pin.rev == rev && pin.commit == commit));
        assert_eq!(rules.0[0], cache_path(&cache, &repo.source(), &commit)?);
    }
    Ok(())
}

#[test]
fn rejects_traversal_and_keeps_source_paths_distinct() -> Result<()> {
    let project = TestDir::new();
    let cache = project.path().join("cache");
    let external = TestDir::new();
    external.write("secret.md", "outside pack");
    let commit = "a".repeat(40);
    let a = cache_path(&cache, "../../some/a/b", &commit)?;
    let b = cache_path(&cache, "../../some/a_b", &commit)?;
    assert_eq!(a.parent(), Some(cache.as_path()));
    assert_ne!(a, b);
    assert!(!a.file_name().unwrap().to_string_lossy().contains(".."));
    assert!(cache_path(&cache, "repo", "../../etc").is_err());
    project.write(
        "fuzukue.lock",
        "[[rules]]\nsource = 'ssh://host/repo'\nrev = 'main'\ncommit = '../../etc'",
    );
    assert!(Lock::read(project.path()).is_err());

    project.write(
        "fuzukue.toml",
        "[[rules]]\nsource = 'github:owner/../repo'\nrev = 'main'",
    );
    assert!(resolve_with_cache(project.path(), || panic!(
        "invalid source must fail before opening cache"
    ))
    .is_err());

    let repo = Repo::new()?;
    repo.configure(&project, "main");
    fs::remove_file(project.path().join("fuzukue.lock"))?;
    let linked = repo.dir.path().join("work/policy");
    fs::create_dir(&linked)?;
    std::os::unix::fs::symlink(external.path().join("secret.md"), linked.join("escaped.md"))?;
    let commit = repo.commit()?;
    let destination = cache_path(&cache, &repo.source(), &commit)?;
    let error = resolve_with_cache(project.path(), || Ok(cache.clone()))
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("invalid rule entry"));
    assert!(!destination.exists());
    assert!(!project.path().join("fuzukue.lock").exists());
    std::os::unix::fs::symlink(external.path(), &destination)?;
    assert!(resolve_with_cache(project.path(), || Ok(cache)).is_err());
    Ok(())
}

#[test]
fn layers_reviewers_style_lint_policy_and_skills() -> Result<()> {
    let project = TestDir::new();
    let first = TestDir::new();
    let second = TestDir::new();
    project.write(
        "fuzukue.toml",
        &format!(
            "[[rules]]\nsource = {:?}\nrev = 'unused'\n[[rules]]\nsource = {:?}\n",
            first.path(),
            second.path()
        ),
    );
    let rules = resolve_with_cache(project.path(), || panic!("local packs do not need a cache"))?;
    for (dir, label) in [(&first, "first"), (&second, "second")] {
        dir.write("reviewers/style.md", label);
        dir.write("policy/shared.md", label);
        dir.write("skills/shared/SKILL.md", label);
    }
    assert_eq!(
        rules.reviewer(project.path(), "style.md")?.as_deref(),
        Some("second")
    );
    project.write("reviewers/style.md", "local reviewer");
    assert_eq!(
        rules.reviewer(project.path(), "style.md")?.as_deref(),
        Some("local reviewer")
    );
    assert_eq!(rules.reviewer(project.path(), "lean.md")?, None);
    assert!(rules.reviewer(project.path(), "../style.md").is_err());

    first.write("policy/retained.md", "first policy");
    second.write("policy/extra.md", "second policy");
    project.write("policy/shared.md", "local policy");
    assert_eq!(
        rules.policy(project.path())?,
        format!(
            "{}\n\nfirst policy\n\nsecond policy\n\nlocal policy",
            crate::usage::policy()
        )
    );

    first.write("skills/shared/stale.txt", "overwritten");
    second.write("skills/extra/SKILL.md", "second skill");
    project.write("skills/shared/SKILL.md", "local skill");
    project.write("skills/shared/references/example.md", "skill asset");
    let skills = rules.skills(project.path())?;
    let staged = TestDir::new();
    skills.copy_to(staged.path())?;
    assert_eq!(
        fs::read_to_string(staged.path().join("shared/SKILL.md"))?,
        "local skill"
    );
    assert_eq!(
        fs::read_to_string(staged.path().join("extra/SKILL.md"))?,
        "second skill"
    );
    assert!(staged.path().join("shared/references/example.md").is_file());
    assert!(!staged.path().join("shared/stale.txt").exists());

    first.write(
        "style/default.toml",
        "phrases = ['first phrase']\nmax_en_words = 1",
    );
    second.write(
        "style/default.toml",
        "[style]\nphrases = ['second phrase']\nmax_en_words = 2",
    );
    first.write("lint/terms.toml", "ignore_terms = ['alpha']");
    second.write("lint/terms.toml", "[lint]\nignore_terms = ['beta']");
    project.write(
        "cards/defines.md",
        "---\nid: defines\nkind: definition\ndefines: [alpha, beta]\n---\n",
    );
    project.write(
        "cards/use.md",
        "---\nid: use\n---\nFirst phrase. Second phrase. Alpha and beta.",
    );
    let diagnostics = || -> Result<Vec<_>> {
        Ok(open_graph(project.path(), project.path())?
            .diagnostics
            .into_iter()
            .filter(|d| d.code.is_some())
            .collect())
    };
    let from_packs = diagnostics()?;
    assert!(from_packs
        .iter()
        .any(|d| d.code == Some("style/ai-phrase")
            && d.message.to_lowercase().contains("second phrase")));
    assert!(!from_packs
        .iter()
        .any(|d| d.code == Some("style/ai-phrase")
            && d.message.to_lowercase().contains("first phrase")));
    assert!(from_packs
        .iter()
        .any(|d| d.code == Some("style/long-sentence")));
    assert!(from_packs.iter().any(
        |d| d.code == Some("undefined-term/missing-dependency") && d.message.contains("alpha")
    ));
    assert!(
        !from_packs
            .iter()
            .any(|d| d.code == Some("undefined-term/missing-dependency")
                && d.message.contains("beta"))
    );

    project.write("style/a.toml", "phrases = ['alpha']\nmax_en_words = 3");
    project.write("style/z.toml", "phrases = ['beta']");
    project.write("lint/local.toml", "ignore_terms = ['alpha', 'beta']");
    let from_project = diagnostics()?;
    assert_eq!(from_project.len(), 1, "{from_project:?}");
    assert!(from_project[0].message.contains("beta"));

    project.write(
        "deck.toml",
        "[lint]\nignore_terms = ['alpha']\n[style]\nphrases = ['first phrase']\nmax_en_words = 2",
    );
    project.write(
        "style.toml",
        "phrases = ['second phrase']\nmax_en_words = 40",
    );
    let from_deck = diagnostics()?;
    assert_eq!(from_deck.len(), 2, "{from_deck:?}");
    assert!(from_deck
        .iter()
        .any(|d| d.code == Some("style/ai-phrase")
            && d.message.to_lowercase().contains("second phrase")));
    assert!(
        from_deck
            .iter()
            .any(|d| d.code == Some("undefined-term/missing-dependency")
                && d.message.contains("beta"))
    );
    assert!(!project.path().join("fuzukue.lock").exists());
    Ok(())
}
