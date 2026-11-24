use fuzukue_proto::GitHubLink;

use super::{url_link, Expected, Kind};
use crate::workshop::valid_repo;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Write {
    Item(GitHubLink, Vec<Expected>),
    Create(String, Vec<Expected>),
}

// shlex handles quoting, but doesn't identify shell operators or expansion.
// Accept a single literal command and the shell wrapper used by exec.
fn literal_words(command: &str) -> Option<Vec<String>> {
    let mut quote = None;
    let mut escaped = false;
    for ch in command.trim().chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => escaped = true,
            (_, '$' | '`') => return None,
            (None, '\'' | '"') => quote = Some(ch),
            (
                None,
                ';' | '&' | '|' | '<' | '>' | '(' | ')' | '\n' | '\r' | '*' | '?' | '[' | ']',
            ) => return None,
            _ => {}
        }
    }
    shlex::split(command)
}

pub(super) fn parse(command: &str, default_repo: &str) -> Option<Write> {
    let words = literal_words(command)?;
    let wrapped;
    let mut args = words.as_slice();
    if let [shell, flag, script] = args {
        if matches!(
            shell.as_str(),
            "bash" | "sh" | "/bin/bash" | "/usr/bin/bash" | "/bin/sh" | "/usr/bin/sh"
        ) && matches!(flag.as_str(), "-c" | "-lc" | "-cl")
        {
            wrapped = literal_words(script)?;
            args = &wrapped;
        }
    }
    let [gh, group, rest @ ..] = args else {
        return None;
    };
    if gh != "gh" {
        return None;
    }
    match group.as_str() {
        "issue" | "pr" => cli(group, rest, default_repo),
        "api" => api(rest, default_repo),
        _ => None,
    }
}

fn option(arg: &str) -> (&str, Option<&str>) {
    if arg.starts_with("--") {
        return arg
            .split_once('=')
            .map_or((arg, None), |(flag, value)| (flag, Some(value)));
    }
    if arg.starts_with('-') && arg.len() > 2 && arg.is_char_boundary(2) {
        return (
            &arg[..2],
            Some(arg[2..].strip_prefix('=').unwrap_or(&arg[2..])),
        );
    }
    (arg, None)
}

fn labels(expected: &mut Vec<Expected>, kind: Kind, csv: &str) {
    expected.extend(
        csv.split(',')
            .filter(|label| !label.is_empty())
            .map(|label| Expected::Label(kind, label.into())),
    );
}

fn cli(group: &str, args: &[String], default_repo: &str) -> Option<Write> {
    let [action, args @ ..] = args else {
        return None;
    };
    let action = action.as_str();
    let mut expected = match action {
        "comment" | "edit" | "label" => Vec::new(),
        "close" => vec![Expected::Activity(Kind::Closed)],
        "reopen" => vec![Expected::Activity(Kind::Reopened)],
        "create" => vec![Expected::Opened],
        _ => return None,
    };
    let mut repo = None;
    let mut target = None;
    let mut args = args.iter();
    let mut comment = None;
    while let Some(arg) = args.next().map(String::as_str) {
        if !arg.starts_with('-') {
            if target.replace(arg).is_some() {
                return None;
            }
            continue;
        }
        let (flag, attached) = option(arg);
        let mut value = || attached.or_else(|| args.next().map(String::as_str));
        match (action, flag) {
            (_, "-R" | "--repo") if repo.is_none() => repo = Some(value()?),
            ("close" | "reopen", "-c" | "--comment") | ("comment", "-b" | "--body") => {
                comment = Some(Some(value()?.to_owned()))
            }
            ("comment", "-F" | "--body-file") => {
                value()?;
                comment = Some(None);
            }
            ("edit" | "label", "--add-label")
            | ("label", "--add" | "-a")
            | ("create", "-l" | "--label") => labels(&mut expected, Kind::Labeled, value()?),
            ("edit" | "label", "--remove-label") | ("label", "--remove" | "-r") => {
                labels(&mut expected, Kind::Unlabeled, value()?);
            }
            ("close", "--reason" | "-r") => {
                value()?;
            }
            ("close", "--delete-branch") if attached.is_none() => {}
            (
                "edit" | "create",
                "--body" | "-b" | "--body-file" | "-F" | "--title" | "-t" | "--milestone" | "-m"
                | "--add-assignee" | "--remove-assignee" | "--add-project" | "--remove-project"
                | "--add-reviewer" | "--remove-reviewer" | "--assignee" | "-a" | "--project" | "-p"
                | "--reviewer" | "-r" | "--base" | "-B" | "--head" | "-H" | "--template" | "-T",
            ) => {
                value()?;
            }
            ("edit", "--remove-milestone")
            | (
                "create",
                "--draft" | "-d" | "--fill" | "-f" | "--fill-first" | "--fill-verbose"
                | "--recover",
            ) if attached.is_none() && flag != "--recover" => {}
            ("create", "--recover") => {
                value()?;
            }
            _ => return None,
        }
    }
    if action == "comment" {
        expected.push(Expected::comment(comment?));
    } else if let Some(body) = comment {
        expected.push(Expected::comment(body));
    }
    if action == "create" {
        let repo = repo.unwrap_or(default_repo);
        return (target.is_none() && valid_repo(repo))
            .then(|| Write::Create(repo.to_ascii_lowercase(), expected));
    }
    let target = target?;
    let link = if let Ok(number) = target.parse::<u64>() {
        let repo = repo.unwrap_or(default_repo);
        (number > 0 && valid_repo(repo)).then(|| GitHubLink {
            repo: repo.to_ascii_lowercase(),
            number,
        })?
    } else {
        let link = url_link(target).ok()?;
        let segment = if group == "issue" { "issues" } else { "pull" };
        if url::Url::parse(target).ok()?.path_segments()?.nth(2) != Some(segment)
            || repo.is_some_and(|repo| !link.repo.eq_ignore_ascii_case(repo))
        {
            return None;
        }
        link
    };
    Some(Write::Item(link, expected))
}

fn api(args: &[String], default_repo: &str) -> Option<Write> {
    let mut args = args.iter();
    let mut method = None;
    let mut path = None;
    let mut fields = Vec::new();
    let mut input = false;
    while let Some(arg) = args.next().map(String::as_str) {
        if !arg.starts_with('-') {
            if path.replace(arg).is_some() {
                return None;
            }
            continue;
        }
        let (flag, attached) = option(arg);
        let mut value = || attached.or_else(|| args.next().map(String::as_str));
        match flag {
            "-X" | "--method" if method.is_none() => method = Some(value()?),
            "-f" | "--raw-field" | "-F" | "--field" => {
                let field = value()?;
                let typed = matches!(flag, "-F" | "--field");
                let (key, data) = match field.split_once('=') {
                    Some((key, data)) => (key, Some(data)),
                    None if typed && field == "labels[]" => (field, None),
                    None => return None,
                };
                fields.push((key, data, typed));
            }
            "--hostname" if value()? == "github.com" => {}
            "--input" => {
                value()?;
                input = true;
            }
            "-H" | "--header" | "--jq" | "-q" => {
                value()?;
            }
            "--silent" | "-i" | "--include" if attached.is_none() => {}
            _ => return None,
        }
    }
    // With --input, field flags become query parameters and the body is opaque.
    if input {
        fields.clear();
    }
    let method = method?;
    let path = path?.trim_start_matches('/');
    let segments: Vec<_> = path.split('/').collect();
    let ["repos", owner, repo, "issues", number, suffix @ ..] = segments.as_slice() else {
        return None;
    };
    let (default_owner, default_name) = default_repo.split_once('/').unwrap_or_default();
    let owner = if *owner == "{owner}" {
        default_owner
    } else {
        owner
    };
    let repo = if *repo == "{repo}" {
        default_name
    } else {
        repo
    };
    let repo = format!("{owner}/{repo}");
    let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
    if !valid_repo(&repo) {
        return None;
    }
    let mut expected = Vec::new();
    let mut replacement = None;
    match (method, suffix) {
        ("POST", ["comments"]) => {
            let body = fields
                .iter()
                .rev()
                .find(|(key, _, _)| *key == "body")
                .and_then(|(_, value, typed)| {
                    value.filter(|text| !*typed || !text.starts_with('@'))
                })
                .map(str::to_owned);
            expected.push(Expected::comment(body));
        }
        ("POST", ["labels"]) | ("PATCH", []) => {
            for (key, value, typed) in fields {
                match (method, key, value) {
                    ("PATCH", "state", Some("closed")) => {
                        expected.push(Expected::Activity(Kind::Closed))
                    }
                    ("PATCH", "state", Some("open")) => {
                        expected.push(Expected::Activity(Kind::Reopened))
                    }
                    ("PATCH", "labels[]", None) => {
                        replacement.get_or_insert_with(Vec::new);
                    }
                    ("PATCH" | "POST", "labels[]", Some(label))
                        if !typed || !label.starts_with('@') =>
                    {
                        if method == "PATCH" {
                            replacement
                                .get_or_insert_with(Vec::new)
                                .push(label.to_owned());
                        }
                        expected.push(Expected::Label(Kind::Labeled, label.to_owned()));
                    }
                    ("PATCH", "labels[]", _) => return None,
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    if let Some(kept) = replacement {
        expected.push(Expected::RemovedLabels(kept));
    }
    Some(Write::Item(
        GitHubLink {
            repo: repo.to_ascii_lowercase(),
            number,
        },
        expected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_literal_writes() {
        let repo = "Owner/Lab";
        let close = || Expected::Activity(Kind::Closed);
        let comment = |body: &str| Expected::comment(Some(body.into()));
        for (command, expected) in [
            ("gh issue close 12 -c 'Done; verified'", vec![close(), comment("Done; verified")]),
            ("gh pr close https://github.com/OWNER/lab/pull/12 --comment='Done <!-- lab -->' --delete-branch",
                vec![close(), comment("Done <!-- lab -->")]),
            ("/bin/bash -lc 'gh issue close 12 --reason completed'", vec![close()]),
            ("gh issue reopen -ROwner/Lab 12", vec![Expected::Activity(Kind::Reopened)]),
            ("gh pr comment 12 -b'Done'", vec![comment("Done")]),
            ("gh issue comment 12 --body-file -", vec![Expected::comment(None)]),
            ("gh issue edit 12 --title 'New title'", vec![]),
            ("gh pr edit 12 --add-label 'bug,good first issue' --remove-label=triage",
                vec![Expected::Label(Kind::Labeled, "bug".into()),
                    Expected::Label(Kind::Labeled, "good first issue".into()),
                    Expected::Label(Kind::Unlabeled, "triage".into())]),
            ("gh issue label 12 --add bug --remove triage",
                vec![Expected::Label(Kind::Labeled, "bug".into()), Expected::Label(Kind::Unlabeled, "triage".into())]),
            ("gh api -X POST 'repos/{owner}/{repo}/issues/12/comments' -f 'body=Done'",
                vec![comment("Done")]),
            ("gh api repos/owner/lab/issues/12 --method=PATCH -f state=closed", vec![close()]),
            ("gh api -XPATCH /repos/owner/lab/issues/12 -f state=open -f 'labels[]=bug'",
                vec![Expected::Activity(Kind::Reopened), Expected::Label(Kind::Labeled, "bug".into()),
                    Expected::RemovedLabels(vec!["bug".into()])]),
            ("gh api -X POST repos/owner/lab/issues/12/labels -f 'labels[]=triage'",
                vec![Expected::Label(Kind::Labeled, "triage".into())]),
            ("gh api -X PATCH repos/owner/lab/issues/12 -F 'labels[]'",
                vec![Expected::RemovedLabels(vec![])]),
            ("gh api -X PATCH repos/owner/lab/issues/12 --input payload.json -f state=closed",
                vec![]),
        ] {
            let link = GitHubLink { repo: "owner/lab".into(), number: 12 };
            assert_eq!(parse(command, repo), Some(Write::Item(link, expected)), "{command}");
        }
        for command in [
            "gh issue create -R owner/lab --title 'New issue' -b 'Body'",
            "gh pr create --title 'New PR' --body 'Body' --base main",
        ] {
            assert_eq!(
                parse(command, repo),
                Some(Write::Create("owner/lab".into(), vec![Expected::Opened])),
                "{command}"
            );
        }
    }

    #[test]
    fn rejects_reads_and_ambiguous_targets() {
        for command in [
            "gh issue view 12",
            "gh issue list",
            "gh pr status",
            "gh api repos/owner/lab/issues/12",
            "gh api -X GET repos/owner/lab/issues/12",
            "gh api -X POST --hostname ghe.example repos/owner/lab/issues/12/comments",
            "gh issue comment 12",
            "gh pr close feature-branch",
            "gh issue close 0",
            "gh issue close https://github.com/owner/lab/issues/12 --repo owner/else",
            "gh issue close 12 13",
            "gh issue close --bogus 12",
            "echo gh issue close 12",
            "gh issue close 12; gh issue close 13",
            "gh issue close 12\ngh issue close 13",
            "gh issue close ${ISSUE}",
            "gh issue comment 12 -b \"$(cat body)\"",
            "cd elsewhere && gh issue close 12",
            "gh issue close 12 --comment 'unclosed",
        ] {
            assert_eq!(parse(command, "owner/lab"), None, "{command}");
        }
    }
}
