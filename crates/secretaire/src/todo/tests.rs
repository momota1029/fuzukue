use super::*;

fn add(text: &str) -> Change {
    Change::Add { text: text.into(), state: None, wait: None, note: None }
}

fn set(id: &str, state: TodoState, note: Option<&str>) -> Change {
    Change::SetState { id: id.into(), state, wait: None, note: note.map(Into::into) }
}

fn project() -> Result<tempfile::TempDir> {
    let project = tempfile::tempdir()?;
    fs::create_dir(project.path().join("threads"))?;
    Ok(project)
}

const SESSION: &str = "thread-1";

#[test]
fn items_are_bounded_numbered_once_and_survive_reloading() -> Result<()> {
    assert!(crate::threads::is_thread(SESSION));
    let project = project()?;
    let path = path(project.path(), SESSION)?;
    assert!(read(&path)?.is_none());
    let mut list = List::default();
    assert_eq!(list.apply(add("  first\n\titem "), 10)?, ("T1".into(), true));
    assert_eq!(list.apply(add("first item"), 11)?, ("T1".into(), false));
    assert!(list.apply(add("  "), 11).is_err());
    assert!(list.apply(add(&"長".repeat(MAX_TEXT + 1)), 11).is_err());
    list.apply(add(&"長".repeat(MAX_TEXT)), 11)?;
    assert!(list.apply(set("T1", TodoState::Done, None), 12).is_err());
    assert!(list.apply(set("T1", TodoState::Done, Some(" ")), 12).is_err());
    assert!(list.apply(set("T1", TodoState::Done, Some(&"x".repeat(MAX_NOTE + 1))), 12).is_err());
    list.apply(set("T1", TodoState::Done, Some("tests passed")), 12)?;
    list.apply(Change::Remove { id: "T2".into() }, 13)?;
    let refused = list.apply(set("T2", TodoState::Todo, None), 14).unwrap_err();
    assert!(refused.to_string().contains("removed"));
    list.save(&path)?;

    let mut reloaded = read(&path)?.context("saved list")?;
    assert_eq!(reloaded.revision, 1);
    assert!(reloaded.items[1].removed);
    assert_eq!(reloaded.apply(add("third"), 20)?.0, "T3");
    let ServerEvent::TodoChanged { items, unreadable, .. } = reloaded.event(SESSION) else { panic!() };
    assert!(!unreadable);
    assert_eq!(items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["T1", "T3"]);

    // A stale counter cannot reuse an ID.
    reloaded.next = 1;
    reloaded.save(&path)?;
    assert_eq!(read(&path)?.unwrap().next, 4);
    Ok(())
}

#[test]
fn open_items_are_capped_and_only_closed_ones_expire() -> Result<()> {
    let mut list = List::default();
    for index in 0..MAX_OPEN {
        list.apply(add(&format!("item {index}")), 1)?;
    }
    let refused = list.apply(add("one too many"), 2).unwrap_err();
    assert!(refused.to_string().contains("100 open items"));
    list.apply(set("T1", TodoState::Done, Some("ok")), 2)?;
    list.apply(add("now it fits"), 3)?;
    assert!(list.apply(set("T1", TodoState::Todo, None), 4).is_err());
    assert_eq!(list.open_count(), MAX_OPEN);

    // Fill the closed sets far past their limits without touching open items.
    let open = list.open_count();
    for index in 0..(MAX_DONE + MAX_REMOVED) * 2 {
        let id = format!("T{}", list.next);
        list.next += 1;
        list.items.push(Stored {
            item: TodoItem { id, text: format!("old {index}"), state: TodoState::Done, wait: None, note: None,
                by: TodoAuthor::Front, created: 0, updated: 1000 + index as i64 },
            removed: index % 2 == 0,
            user_change: None,
        });
    }
    list.prune();
    assert_eq!(list.open_count(), open);
    assert_eq!(list.items.iter().filter(|stored| stored.removed).count(), MAX_REMOVED);
    assert_eq!(list.items.iter().filter(|stored| !stored.removed && stored.item.state == TodoState::Done).count(), MAX_DONE);
    assert!(list.items.iter().any(|stored| stored.item.text == format!("old {}", (MAX_DONE + MAX_REMOVED) * 2 - 1)));
    Ok(())
}

#[test]
fn the_largest_list_still_fits_its_file() -> Result<()> {
    let project = project()?;
    let path = path(project.path(), SESSION)?;
    let mut list = List::default();
    for index in 0..MAX_OPEN + MAX_DONE + MAX_REMOVED {
        list.items.push(Stored {
            item: TodoItem { id: format!("T{}", index + 1), text: "\u{1f600}".repeat(MAX_TEXT),
                state: if index < MAX_OPEN { TodoState::Waiting } else { TodoState::Done },
                wait: Some(TodoWait::Action), note: Some("\u{1f600}".repeat(MAX_NOTE)),
                by: TodoAuthor::Front, created: i64::MAX, updated: i64::MAX },
            removed: index >= MAX_OPEN + MAX_DONE,
            user_change: Some(UserChange::Reopened),
        });
    }
    list.save(&path)?;
    assert!(read(&path)?.is_some());
    Ok(())
}

#[test]
fn damaged_foreign_and_linked_files_are_never_replaced() -> Result<()> {
    let project = project()?;
    let path = path(project.path(), SESSION)?;
    fs::create_dir(path.parent().unwrap())?;
    for damaged in [
        "{".to_owned(),
        r#"{"version":2,"revision":1,"next":1,"items":[]}"#.to_owned(),
        r#"{"version":1,"revision":1,"next":1,"items":[{"id":"x","text":"a","state":"todo","by":"front","created":0,"updated":0}]}"#.to_owned(),
        " ".repeat(MAX_FILE as usize + 1),
    ] {
        fs::write(&path, &damaged)?;
        assert!(read(&path).is_err());
        assert_eq!(fs::read_to_string(&path)?, damaged);
    }
    #[cfg(unix)]
    {
        let outside = project.path().join("outside.json");
        fs::write(&outside, "{}")?;
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink(&outside, &path)?;
        assert!(super::path(project.path(), SESSION).is_err());
        assert!(purge(project.path(), SESSION).is_err());
        assert_eq!(fs::read_to_string(&outside)?, "{}");
    }
    for foreign in ["../escape", "thread-/../../escape", "thread-a/b", "thread-a\\b", "thread-a.b"] {
        assert!(super::path(project.path(), foreign).is_err(), "{foreign}");
    }
    Ok(())
}

#[test]
fn purging_removes_the_list_and_keeps_the_goal() -> Result<()> {
    let project = project()?;
    let path = path(project.path(), SESSION)?;
    purge(project.path(), SESSION)?;
    List::default().save(&path)?;
    let plan = path.with_file_name("plan.md");
    fs::write(&plan, "goal")?;
    purge(project.path(), SESSION)?;
    assert!(!path.exists() && plan.is_file());
    fs::remove_file(&plan)?;
    List::default().save(&path)?;
    purge(project.path(), SESSION)?;
    assert!(!path.parent().unwrap().exists());
    Ok(())
}

fn filled(count: usize, text: &str) -> Result<List> {
    let mut list = List::default();
    for index in 0..count {
        list.apply(add(&format!("{index} {text}")), 1)?;
    }
    Ok(list)
}

#[test]
fn the_turn_preamble_stays_within_its_caps() -> Result<()> {
    assert_eq!(render(&List::default()).0.lines().count(), 1);
    for text in ["short".to_owned(), "とても長い依頼".repeat(27)] {
        let mut list = filled(MAX_OPEN, &text)?;
        // IDs far into a long-lived thread are longer.
        for stored in &mut list.items {
            let number = number(&stored.item.id).unwrap() + 1_000_000_000;
            stored.item.id = format!("T{number}");
            stored.user_change = Some(if number % 2 == 0 { UserChange::Done } else { UserChange::Reopened });
        }
        let (preamble, notices) = render(&list);
        assert!(preamble.chars().count() <= PREAMBLE_CHARS, "{}", preamble.chars().count());
        // Only the changes named in the block are cleared.
        let named = notices.0.len();
        assert!(named > 0 && named < MAX_OPEN);
        assert!(notices.0.iter().all(|(id, _)| preamble.contains(id.as_str())));
        assert!(list.clear(&notices));
        assert_eq!(list.items.iter().filter(|stored| stored.user_change.is_some()).count(), MAX_OPEN - named);
        let rows: Vec<_> = preamble.lines().filter(|row| row.contains('[')).collect();
        assert!(!rows.is_empty() && rows.len() <= PREAMBLE_ITEMS);
        assert!(rows.iter().all(|row| row.chars().count() <= PREAMBLE_LINE));
        let omitted = MAX_OPEN - rows.len();
        assert!(preamble.contains(&format!("{omitted} more")) || preamble.contains(&format!("ほか {omitted} 件")));
    }
    let preamble = render(&filled(MAX_OPEN, "x")?).0;
    assert_eq!(preamble.lines().filter(|row| row.contains('[')).count(), PREAMBLE_ITEMS);
    assert!(preamble.contains("T21 T22"));
    Ok(())
}

#[test]
fn the_preamble_lists_running_then_waiting_then_oldest_items() -> Result<()> {
    let mut list = filled(5, "item")?;
    list.apply(set("T4", TodoState::Doing, None), 2)?;
    list.apply(set("T3", TodoState::Waiting, None), 2)?;
    list.apply(set("T1", TodoState::Done, Some("verified")), 2)?;
    let preamble = render(&list).0;
    let ids: Vec<_> = preamble.lines().skip(1).map(|row| row.split(' ').next().unwrap()).collect();
    assert_eq!(ids, ["T4", "T3", "T2", "T5"]);
    assert_eq!(list.items[2].item.wait, Some(TodoWait::Reply));
    Ok(())
}
