use std::cell::RefCell;

use anyhow::bail;
use url::Url;

use super::*;

const ZB: &[u8] = include_bytes!("../../../tests/fixtures/references/zbmath.json");
const ARXIV: &[u8] = include_bytes!("../../../tests/fixtures/references/arxiv.xml");
const CROSSREF: &[u8] = include_bytes!("../../../tests/fixtures/references/crossref.json");
const PDF: &[u8] = b"%PDF-1.4\nfixture";

#[derive(Default)]
struct Canned {
    requests: RefCell<Vec<Url>>,
    pdf: Option<&'static [u8]>,
    zb_unavailable: bool,
}

impl Http for Canned {
    fn get(&self, url: &Url, accept: &str, _: usize) -> Result<Option<Vec<u8>>> {
        self.requests.borrow_mut().push(url.clone());
        match (url.host_str().unwrap(), url.path(), accept) {
            ("api.zbmath.org", _, _) if self.zb_unavailable => bail!("fixture unavailable"),
            ("api.zbmath.org", "/v1/document/_search", "application/json") => Ok(Some(ZB.into())),
            ("api.zbmath.org", "/v1/document/2345678", "application/json") => {
                let data: Value = serde_json::from_slice(ZB)?;
                Ok(Some(serde_json::to_vec(
                    &json!({"result": data["result"][0]}),
                )?))
            }
            ("export.arxiv.org", "/api/query", "application/atom+xml") => Ok(Some(ARXIV.into())),
            ("api.crossref.org", "/works", "application/json") => Ok(Some(CROSSREF.into())),
            ("api.crossref.org", path, "application/json") if path.starts_with("/works/") => {
                let data: Value = serde_json::from_slice(CROSSREF)?;
                let items = data["message"]["items"].as_array().unwrap();
                let index = match path {
                    "/works/10.1234%2Fconcentration" => 0,
                    "/works/10.1234%2Fgeometry" => 1,
                    _ => panic!("unexpected DOI request: {url}"),
                };
                Ok(Some(serde_json::to_vec(&json!({"message": items[index]}))?))
            }
            ("arxiv.org", "/pdf/2301.01234v2", "application/pdf") => Ok(self.pdf.map(Vec::from)),
            _ => panic!("unexpected request: {url} ({accept})"),
        }
    }
}

fn import(http: &Canned, project: &Path, id: &str, key: Option<&str>) -> Result<Value> {
    store(project, download(http, &Identifier::parse(id)?)?, key)
}

#[test]
fn finds_candidates_from_each_source() -> Result<()> {
    let http = Canned::default();
    let result = find_with(&http, "concentration & geometry", Source::All, 6)?;
    let papers = result["candidates"].as_array().unwrap();
    assert_eq!(papers.len(), 6);
    assert_eq!(papers[0]["id"], "zbmath:1234.56789");
    assert_eq!(papers[0]["title"], "A concentration theorem");
    assert_eq!(
        papers[0]["authors"],
        json!(["Lédoux, Michel", "Noether, Emmy"])
    );
    assert_eq!(papers[0]["year"], 2001);
    assert_eq!(papers[0]["venue"], "Journal of Fixture Mathematics");
    assert_eq!(papers[0]["ids"]["mr"], "MR1234567");
    assert_eq!(papers[0]["ids"]["doi"], "10.1234/concentration");
    assert_eq!(papers[0]["open_pdf"], false);
    assert_eq!(papers[1]["id"], "arxiv:2301.01234v2");
    assert_eq!(papers[1]["title"], "Concentration & geometry");
    assert_eq!(papers[1]["authors"][0], "Emmy Noether");
    assert_eq!(papers[1]["year"], 2023);
    assert_eq!(papers[1]["venue"], "Journal of Fixture Mathematics");
    assert_eq!(papers[1]["ids"]["doi"], "10.1234/geometry");
    assert_eq!(papers[1]["open_pdf"], true);
    assert_eq!(papers[2]["id"], "doi:10.1234/concentration");
    assert_eq!(
        papers[2]["authors"],
        json!(["Lédoux, Michel", "A research team"])
    );
    assert_eq!(papers[2]["year"], 2001);
    assert_eq!(papers[2]["venue"], "Journal of Fixture Mathematics");
    assert_eq!(papers[2]["open_pdf"], false);
    assert_eq!(papers[3]["ids"]["arxiv"], "2301.01234v2");
    assert_eq!(papers[3]["open_pdf"], true);
    assert_eq!(papers[4]["ids"]["arxiv"], "math.gt/0301001v1");
    assert_eq!(papers[4]["open_pdf"], false);
    assert_eq!(papers[5]["open_pdf"], true);
    let requests = http.requests.borrow();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0]
            .query_pairs()
            .find(|(key, _)| key == "search_string")
            .unwrap()
            .1,
        "concentration & geometry"
    );
    assert_eq!(
        requests[1]
            .query_pairs()
            .find(|(key, _)| key == "max_results")
            .unwrap()
            .1,
        "6"
    );
    assert_eq!(
        requests[2]
            .query_pairs()
            .find(|(key, _)| key == "query.bibliographic")
            .unwrap()
            .1,
        "concentration & geometry"
    );
    Ok(())
}

#[test]
fn uses_unique_keys_and_updates_only_a_matching_entry() -> Result<()> {
    let project = tempfile::tempdir()?;
    let root = project.path().join("references");
    fs::create_dir_all(root.join("ledoux2001"))?;
    fs::write(root.join("ledoux2001/ref.toml"), "title = 'Unrelated'\n")?;
    let http = Canned::default();

    let first = import(&http, project.path(), "zbmath:1234.56789", None)?;
    assert_eq!(first["key"], "ledoux2001-2");
    assert_eq!(first["pdf"], Value::Null);
    let dir = root.join("ledoux2001-2");
    let mut metadata = fs::read_to_string(dir.join("ref.toml"))?;
    metadata.push_str("\nbib_key = 'manualKey'\ncustom = 'keep this'\n");
    fs::write(dir.join("ref.toml"), metadata)?;
    fs::write(dir.join("note.txt"), "An existing note\n")?;

    let second = import(&http, project.path(), "doi:10.1234/CONCENTRATION", None)?;
    assert_eq!(second["key"], first["key"]);
    assert_eq!(second["corpus"]["rebuilt"], 1);
    assert_eq!(second["pdf"], Value::Null);
    let table = read_existing(&dir).unwrap();
    assert_eq!(table["doi"].as_str(), Some("10.1234/concentration"));
    assert_eq!(table["mr"].as_str(), Some("MR1234567"));
    assert_eq!(table["zbmath"].as_str(), Some("1234.56789"));
    assert_eq!(table["custom"].as_str(), Some("keep this"));
    assert!(table["bibtex"]
        .as_str()
        .unwrap()
        .starts_with("@article{manualKey,"));
    assert!(table["bibtex"]
        .as_str()
        .unwrap()
        .contains("Lédoux, Michel and A research team"));
    assert!(fs::read_to_string(root.join("ledoux2001/ref.toml"))?.contains("Unrelated"));
    assert!(import(
        &http,
        project.path(),
        "doi:10.1234/concentration",
        Some("ledoux2001")
    )
    .is_err());
    assert!(import(
        &http,
        project.path(),
        "zbmath:2345678",
        Some("ledoux2001-2")
    )
    .is_ok());
    assert!(http
        .requests
        .borrow()
        .iter()
        .all(|url| url.host_str() != Some("publisher.example")));
    Ok(())
}

#[test]
fn resolves_mr_through_zbmath_and_preserves_unmatched_numbers() -> Result<()> {
    let project = tempfile::tempdir()?;
    let http = Canned::default();
    let resolved = import(&http, project.path(), "mr:MR1234567", None)?;
    assert_eq!(resolved["key"], "ledoux2001");
    let root = project.path().join("references");
    assert!(read_existing(&root.join("ledoux2001"))
        .unwrap()
        .contains_key("title"));

    let stub = import(&http, project.path(), "mr:mr7654321", None)?;
    assert_eq!(stub["key"], "mr7654321");
    assert!(stub["warning"].as_str().unwrap().contains("MR7654321"));
    let table = read_existing(&root.join(stub["key"].as_str().unwrap())).unwrap();
    assert!(!table.contains_key("title"));
    assert_eq!(table["mr"].as_str(), Some("MR7654321"));
    assert_eq!(
        table["urls"][0].as_str(),
        Some("https://mathscinet.ams.org/mathscinet-getitem?mr=MR7654321")
    );
    let requests = http.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|url| url.host_str() == Some("api.zbmath.org")));
    assert_eq!(
        requests[0]
            .query_pairs()
            .find(|(key, _)| key == "search_string")
            .unwrap()
            .1,
        "en:MR1234567"
    );

    let offline = Canned {
        zb_unavailable: true,
        ..Canned::default()
    };
    let fallback = import(&offline, project.path(), "mr:MR7654321", None)?;
    assert_eq!(fallback["key"], stub["key"]);
    assert!(fallback["warning"].as_str().unwrap().contains("MR"));
    Ok(())
}

#[test]
fn imports_only_arxiv_pdfs_and_keeps_metadata_when_pdf_fetch_fails() -> Result<()> {
    let project = tempfile::tempdir()?;
    let bad_pdf = Canned {
        pdf: Some(b"<html>Not a PDF</html>"),
        ..Canned::default()
    };
    let result = import(&bad_pdf, project.path(), "arXiv:2301.01234", None)?;
    assert_eq!(result["key"], "noether2023");
    assert_eq!(result["pdf"], Value::Null);
    assert!(result["warning"].as_str().unwrap().contains("PDF"));
    assert!(project
        .path()
        .join(result["path"].as_str().unwrap())
        .is_file());

    let http = Canned {
        pdf: Some(PDF),
        ..Canned::default()
    };
    let result = import(&http, project.path(), "arXiv:2301.01234", None)?;
    let pdf = project.path().join(result["pdf"].as_str().unwrap());
    assert_eq!(fs::read(&pdf)?, PDF);
    let dir = pdf.parent().unwrap();
    let table = read_existing(dir).unwrap();
    assert_eq!(table["arxiv"].as_str(), Some("2301.01234v2"));
    assert!(table["bibtex"]
        .as_str()
        .unwrap()
        .contains(r"Concentration \& geometry"));
    let linked = import(&http, project.path(), "doi:10.1234/geometry", None)?;
    assert_eq!(linked["pdf"], result["pdf"]);
    let withdrawn = import(&http, project.path(), "arXiv:math.GT/0301001", None)?;
    assert_eq!(withdrawn["pdf"], Value::Null);
    let requests = http.requests.borrow();
    let pdf_requests: Vec<_> = requests
        .iter()
        .filter(|url| url.path().starts_with("/pdf/"))
        .collect();
    assert_eq!(pdf_requests.len(), 2);
    assert!(pdf_requests
        .iter()
        .all(|url| url.as_str() == "https://arxiv.org/pdf/2301.01234v2"));
    Ok(())
}

#[test]
fn refuses_traversal_and_symlinks() -> Result<()> {
    let project = tempfile::tempdir()?;
    let http = Canned::default();
    for key in [
        "../outside",
        "a/../b",
        r"..\outside",
        "/tmp/absolute",
        ".",
        "..",
        "",
    ] {
        let fetched = download(&http, &Identifier::parse("mr:7654321")?)?;
        assert!(store(project.path(), fetched, Some(key)).is_err(), "{key}");
    }
    assert!(!project.path().join("references").exists());

    #[cfg(unix)]
    {
        let outside = tempfile::tempdir()?;
        fs::create_dir(project.path().join("references"))?;
        std::os::unix::fs::symlink(outside.path(), project.path().join("references/linked"))?;
        assert!(import(&http, project.path(), "mr:1234567", Some("linked")).is_err());
        assert_eq!(fs::read_dir(outside.path())?.count(), 0);
    }
    Ok(())
}
