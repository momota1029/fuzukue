use super::*;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn card(root: &Path, id: &str, extra: &str, tex: &str) {
    write(root, &format!("cards/{id}/card.md"), &format!("---\nid: {id}\nkind: expl\ntitle: t\n{extra}---\n"));
    write(root, &format!("cards/{id}/statement.tex"), tex);
}

/// Book in the format `import_latex_book` writes: no section keys, one labelled chapter.
fn book(project: &Path) {
    let b = project.join("books/b");
    write(&b, "deck.toml", "kind = \"book\"\ntitle = \"B\"\npreamble = \"preamble.tex\"\n");
    write(&b, "preamble.tex", "\\newcommand{\\R}{\\mathbb R}\n");
    write(
        &b,
        "structure.yaml",
        "title: B\nsections:\n- title: Intro\n  level: chapter\n  tex: ''\n  items: [a1, a2]\n\
         - title: Limits\n  level: chapter\n  label: ch:limits\n  tex: ''\n  items: [b1, {heading: {title: x, level: s, tex: ''}}, {proof: b1p}]\n\
         - title: End\n  level: chapter\n  tex: ''\n  items: [c1]\n",
    );
    card(&b, "a1", "", "A one.");
    card(&b, "a2", "", "A two \\ref{lem:b}.");
    card(&b, "b1", "label: lem:b\n", "B one.");
    card(&b, "b1p", "", "B proof.");
    card(&b, "c1", "", "C one.");
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().to_owned();
    book(&project);
    let lecture = project.join("lec");
    write(&lecture, "deck.toml", "kind = \"notes\"\n");
    (dir, project, lecture)
}

fn adopt(keys: &[&str]) -> Vec<Decision> {
    keys.iter().map(|k| Decision { key: (*k).into(), status: Some(Status::Adopted), exercises: BTreeMap::new() }).collect()
}

fn lecture_card(lecture: &Path, id: &str, from: &str) {
    card(lecture, id, &format!("from: {from}\n"), "lecture.");
}

fn dir_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| (rel(root, e.path()), fs::read(e.path()).unwrap()))
        .collect();
    out.sort();
    out
}

#[test]
fn keys_come_from_label_or_position() {
    let (_d, project, _l) = setup();
    let keys: Vec<_> = book_sections(&project.join("books/b")).unwrap().into_iter().map(|s| s.key).collect();
    assert_eq!(keys, ["s1-intro", "ch-limits", "s3-end"]);
    let cards: Vec<_> = book_sections(&project.join("books/b")).unwrap()[1].cards.clone();
    assert_eq!(cards, ["b1", "b1p"]);
}

#[test]
fn plan_leaves_the_book_untouched_and_resumes_after_interruption() {
    let (_d, project, lecture) = setup();
    let before = dir_bytes(&project.join("books/b"));
    let report = plan(&project, &lecture, Some("books/b"), &adopt(&["s1-intro", "ch-limits"])).unwrap();
    assert!(report.changed);
    assert_eq!(report.next.as_deref(), Some("s1-intro"));
    assert_eq!(dir_bytes(&project.join("books/b")), before);

    let first = next(&project, &lecture).unwrap().section.unwrap();
    assert_eq!((first.key.as_str(), first.resumed, first.card_prefix.as_str()), ("s1-intro", false, "lec-s1-intro"));
    assert_eq!(first.from, "books/b#s1-intro");
    // One card written, then the process stops. A new call returns the same section.
    lecture_card(&lecture, "lec-s1-intro", &first.from);
    let again = next(&project, &lecture).unwrap().section.unwrap();
    assert!(again.resumed);
    assert_eq!(again.key, "s1-intro");
    assert_eq!(again.book_version, first.book_version);
    assert_eq!(again.existing_cards, ["lec-s1-intro"]);

    lecture_card(&lecture, "lec-s1-intro.2", &first.from);
    let cards = ["lec-s1-intro".to_owned(), "lec-s1-intro.2".to_owned()];
    let done = commit(&lecture, "s1-intro", &cards, |_| Ok(())).unwrap();
    assert!(done.changed);
    let saved = fs::read(lecture.join(FILE)).unwrap();
    // Same commit again (even reordered): nothing changes, no new cards.
    let rev = [cards[1].clone(), cards[0].clone()];
    assert!(!commit(&lecture, "s1-intro", &rev, |_| Ok(())).unwrap().changed);
    assert_eq!(fs::read(lecture.join(FILE)).unwrap(), saved);
    assert!(commit(&lecture, "s1-intro", &cards[..1], |_| Ok(())).is_err());

    let second = next(&project, &lecture).unwrap().section.unwrap();
    assert_eq!((second.key.as_str(), second.resumed), ("ch-limits", false));
    assert_eq!(Course::read(&lecture).unwrap().unwrap().next.as_deref(), Some("ch-limits"));
    lecture_card(&lecture, "lec-ch-limits", &second.from);
    commit(&lecture, "ch-limits", &["lec-ch-limits".into()], |_| Ok(())).unwrap();
    assert!(next(&project, &lecture).unwrap().section.is_none());
    // The first section is untouched by the second.
    let course = Course::read(&lecture).unwrap().unwrap();
    assert_eq!(course.sections[0].cards, cards);
}

#[test]
fn commit_validates_cards() {
    let (_d, project, lecture) = setup();
    plan(&project, &lecture, Some("books/b"), &adopt(&["s1-intro"])).unwrap();
    assert!(commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).is_err(), "not writing yet");
    next(&project, &lecture).unwrap();
    assert!(commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).is_err(), "card missing");
    lecture_card(&lecture, "lec-s1-intro", "books/b#other");
    assert!(commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).is_err(), "wrong from");
    lecture_card(&lecture, "lec-s1-intro", "books/b#s1-intro");
    lecture_card(&lecture, "lec-s1-intro.2", "books/b#s1-intro");
    assert!(commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).is_err(), "leftover card");
    assert!(commit(&lecture, "s1-intro", &["lec-s1-intro".into(), "lec-s1-intro.2".into()], |_| bail!("style")).is_err());
    assert_eq!(Course::read(&lecture).unwrap().unwrap().sections[0].status, Status::Writing);
}

#[test]
fn next_stops_on_inconsistent_state() {
    let (_d, project, lecture) = setup();
    plan(&project, &lecture, Some("books/b"), &adopt(&["s1-intro"])).unwrap();
    let first = next(&project, &lecture).unwrap().section.unwrap();
    lecture_card(&lecture, "lec-s1-intro", &first.from);
    commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).unwrap();
    fs::remove_dir_all(lecture.join("cards")).unwrap();
    let error = next(&project, &lecture).unwrap_err().to_string();
    assert!(error.contains("不整合") && error.contains("lec-s1-intro"), "{error}");
}

#[test]
fn plan_rules_and_book_changes() {
    let (_d, project, lecture) = setup();
    plan(&project, &lecture, Some("books/b"), &adopt(&["s1-intro"])).unwrap();
    assert!(plan(&project, &lecture, Some("books/other"), &[]).is_err());
    let bad = [Decision { key: "s1-intro".into(), status: Some(Status::Done), exercises: BTreeMap::new() }];
    assert!(plan(&project, &lecture, None, &bad).is_err());
    next(&project, &lecture).unwrap();
    let skip = [Decision { key: "s1-intro".into(), status: Some(Status::Skipped), exercises: BTreeMap::new() }];
    assert!(plan(&project, &lecture, None, &skip).is_err(), "writing section is not re-planned");
    // Editing a book card marks the section stale without moving its state.
    write(&project.join("books/b"), "cards/a1/statement.tex", "A one, edited.");
    let report = plan(&project, &lecture, None, &[]).unwrap();
    let section = &report.sections[0];
    assert!(section.stale);
    assert_eq!(section.status, Status::Writing);
    assert!(!report.sections[1].stale);
}

#[test]
fn versions_follow_captured_bytes() {
    let (_d, project, _l) = setup();
    let b = project.join("books/b");
    let key = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let s = Snapshot::read(&b).unwrap();
    let a = key(&["a1", "a2"]);
    let (content, math, output) =
        (s.content_version(&a).unwrap(), s.math_version(&a).unwrap(), s.output_version(&a, &[]).unwrap());
    assert!(content.starts_with(VERSION_PREFIX));
    assert_ne!(content, math);

    // A whitespace-only edit changes bytes, so the version (a normalized fingerprint would not).
    write(&b, "cards/a1/statement.tex", "A one.\n");
    let s = Snapshot::read(&b).unwrap();
    assert_ne!(s.content_version(&a).unwrap(), content);
    write(&b, "cards/a1/statement.tex", "A one.");

    // a2 refers to lem:b, defined in b1: a prerequisite edit moves math but not content.
    write(&b, "cards/b1/statement.tex", "B one, edited.");
    let s = Snapshot::read(&b).unwrap();
    assert_eq!(s.content_version(&a).unwrap(), content);
    assert_ne!(s.math_version(&a).unwrap(), math);
    assert_eq!(s.output_version(&a, &[]).unwrap(), output);

    // Preamble and earlier sections move the output version only.
    write(&b, "preamble.tex", "\\newcommand{\\R}{\\mathbf R}\n");
    let s = Snapshot::read(&b).unwrap();
    assert_ne!(s.output_version(&a, &[]).unwrap(), output);
    let later = key(&["c1"]);
    let one = s.output_version(&later, &[key(&["a1", "a2"])]).unwrap();
    write(&b, "cards/a1/statement.tex", "changed");
    let s = Snapshot::read(&b).unwrap();
    assert_ne!(s.output_version(&later, &[key(&["a1", "a2"])]).unwrap(), one);
    assert_eq!(s.math_version(&later).unwrap(), Snapshot::read(&b).unwrap().math_version(&later).unwrap());
}

#[test]
fn input_files_are_captured() {
    let (_d, project, _l) = setup();
    let b = project.join("books/b");
    write(&b, "cards/c1/statement.tex", "C \\input{extra}");
    let ids = vec!["c1".to_owned()];
    let missing = Snapshot::read(&b).unwrap().content_version(&ids).unwrap();
    write(&b, "cards/c1/extra.tex", "one");
    let one = Snapshot::read(&b).unwrap().content_version(&ids).unwrap();
    write(&b, "cards/c1/extra.tex", "two");
    let two = Snapshot::read(&b).unwrap().content_version(&ids).unwrap();
    assert!(missing != one && one != two);
}

#[test]
fn done_requires_current_clean_reviews() {
    let (_d, project, lecture) = setup();
    plan(&project, &lecture, Some("books/b"), &adopt(&["s1-intro"])).unwrap();
    let first = next(&project, &lecture).unwrap().section.unwrap();
    lecture_card(&lecture, "lec-s1-intro", &first.from);
    commit(&lecture, "s1-intro", &["lec-s1-intro".into()], |_| Ok(())).unwrap();
    let done = [Decision { key: "s1-intro".into(), status: Some(Status::Done), exercises: BTreeMap::new() }];
    assert!(plan(&project, &lecture, None, &done).is_err(), "no reviews yet");

    let snapshot = Snapshot::read(&lecture).unwrap();
    let ids = vec!["lec-s1-intro".to_owned()];
    let mut course = Course::read(&lecture).unwrap().unwrap();
    for (kind, version) in [
        ("math", snapshot.math_version(&ids).unwrap()),
        ("output", snapshot.output_version(&ids, &[]).unwrap()),
    ] {
        course.sections[0].reviews.insert(kind.into(), Review { version, requested_at: None, status: ReviewStatus::Clean, request: None });
    }
    course.save(&lecture).unwrap();
    // Editing a lecture card makes both reviews stale.
    write(&lecture, "cards/lec-s1-intro/statement.tex", "edited.");
    let report = plan(&project, &lecture, None, &[]).unwrap();
    assert_eq!(report.sections[0].stale_reviews, ["math", "output"]);
    assert!(plan(&project, &lecture, None, &done).is_err());
    // After a re-review at the current version it can be done.
    let snapshot = Snapshot::read(&lecture).unwrap();
    let mut course = Course::read(&lecture).unwrap().unwrap();
    course.sections[0].reviews.get_mut("math").unwrap().version = snapshot.math_version(&ids).unwrap();
    course.sections[0].reviews.get_mut("output").unwrap().version = snapshot.output_version(&ids, &[]).unwrap();
    course.save(&lecture).unwrap();
    plan(&project, &lecture, None, &done).unwrap();
}

#[test]
fn corrupt_course_toml_is_reported_not_repaired() {
    let (_d, project, lecture) = setup();
    write(&lecture, FILE, "schema = 1\nbook = ");
    assert!(next(&project, &lecture).is_err());
    assert_eq!(fs::read_to_string(lecture.join(FILE)).unwrap(), "schema = 1\nbook = ");
}
