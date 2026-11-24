use super::*;
use fuzukue_proto::{LedgerKind, LedgerLocationKind};

fn entry(key: &str, title: &str, reason: &str) -> LedgerEntry {
    LedgerEntry {
        key: key.into(),
        title: title.into(),
        kind: LedgerKind::Paper,
        reason: reason.into(),
        locations: propose::extract(reason),
        status: LedgerStatus::Pending,
        attempts: Vec::new(),
        acquired: None,
    }
}

#[test]
fn extracts_ordered_identifiers_and_canonicalizes_urls() {
    let ids = propose::extract(
        "https://doi.org/10.1000/A(B). doi:10.1000/a(b), arXiv:HEP-TH/9901001v2; \
         https://arxiv.org/pdf/2401.12345.pdf, PMC123.2 GSE120 GPL42 GSM42 GDS8, \
         https://zenodo.org/records/21 zenodo:21 github.com/org/repo, \
         [site](https://example.org/a).",
    );
    use LedgerLocationKind::*;
    let pairs: Vec<_> = ids
        .iter()
        .map(|loc| (loc.kind, loc.value.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            (Doi, "10.1000/a(b)"),
            (Arxiv, "hep-th/9901001v2"),
            (Arxiv, "2401.12345"),
            (Pmc, "PMC123.2"),
            (Geo, "GSE120"),
            (Geo, "GPL42"),
            (Geo, "GSM42"),
            (Geo, "GDS8"),
            (Zenodo, "21"),
            (Github, "https://github.com/org/repo"),
            (Url, "https://example.org/a"),
        ]
    );
}

#[test]
fn ledger_round_trip_and_existing_reference() -> Result<()> {
    let project = tempfile::tempdir()?;
    let path = project.path();
    let mut ledger = Ledger {
        entries: vec![
            entry("z", "Unknown", "PMC123"),
            entry("a", "Paper", "doi:10.1000/example"),
        ],
    };
    let dir = path.join("references/already-here");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("ref.toml"),
        "doi = '10.1000/example'\nlicense = 'CC0-1.0'\n",
    )?;
    assert!(!ledger.reconcile(path)?);
    fs::write(dir.join("paper.txt"), "the article")?;
    assert!(ledger.reconcile(path)?);
    ledger.save(path)?;
    let bytes = fs::read(path.join("references/ledger.toml"))?;
    let mut again = Ledger::load(path)?;
    let first = &again.entries[0];
    assert_eq!(first.key, "a");
    assert_eq!(first.status, LedgerStatus::Acquired);
    let file = first.acquired.as_ref().unwrap();
    assert_eq!(file.sha256, hash(&b"the article"[..])?);
    assert_eq!(file.file, "references/already-here/paper.txt");
    assert_eq!(file.license.as_deref(), Some("CC0-1.0"));
    again.save(path)?;
    assert_eq!(bytes, fs::read(path.join("references/ledger.toml"))?);
    Ok(())
}

#[test]
fn download_matching_only_considers_requested_entries_and_reports_ambiguity() {
    let mut first = entry("a", "Concentration on the Sphere", "10.1000/sphere");
    first.status = LedgerStatus::AskUser;
    let mut second = entry("b", "Levy concentration phenomena", "10.1000/levy");
    second.status = LedgerStatus::AskUser;
    let mut third = first.clone();
    third.key = "c".into();
    third.status = LedgerStatus::Abandoned;
    let entries = vec![first, second, third];
    assert_eq!(
        downloads::matches(&entries, "else.pdf", "doi:10.1000/sphere"),
        ["a"]
    );
    assert_eq!(
        downloads::matches(&entries, "concentration_on-the-sphere.pdf", ""),
        ["a"]
    );
    assert_eq!(
        downloads::matches(
            &entries,
            "else.pdf",
            "Concentration on the\n Sphere --- doi:10.1000/levy"
        ),
        ["a", "b"]
    );
    assert!(downloads::matches(&entries, "unrelated.zip", "").is_empty());
}
