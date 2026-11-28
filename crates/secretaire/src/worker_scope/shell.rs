//! Recognize a small shell grammar and issue a one-call approval for its literal commands.

use std::path::{Component, Path, PathBuf};

use fuzukue_proto::{Actor, ServerEvent};
use serde_json::{json, Value};

use crate::Hub;

use super::Scope;

const SAVABLE_PREFIXES: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "stat", "sed -n", "grep", "rg", "find", "jq",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Join {
    Pipe,
    And,
}

/// Every stage of these commands only prints a path, a listing, or file
/// contents, or waits. R-65 chose them as the first unattended subset.
const BASIC_READS: &[&str] = &["pwd", "ls", "cat", "sleep"];

pub(crate) struct ApprovedShell {
    pub(crate) input: Value,
    pub(crate) prefix: Option<&'static str>,
    pub(crate) command: String,
    rule: String,
    /// Every stage is in BASIC_READS.
    pub(crate) basic: bool,
    /// Every file operand is in the job's own temporary directory, and no stage reads anything else.
    pub(crate) own_files: bool,
    /// Every stage creates directories in the checkout or job directory, or changes into one.
    pub(crate) creates_directories: bool,
}

impl ApprovedShell {
    pub(crate) fn audit(&self, hub: &Hub, session: &str, job: Option<&str>, id: &str, who: &str) {
        let name = if job.is_some() {
            "worker_approval"
        } else {
            "shell_approval"
        };
        let item = format!("{}:approval:{id}", job.unwrap_or(session));
        let reason =
            "The shell allowlist matched every command and checked its filesystem operands.";
        hub.publish(ServerEvent::ToolCall {
            session: session.into(),
            item: item.clone(),
            actor: Actor::System,
            name: name.into(),
            input: json!({
                "automatic": true, "decision": "accept", "decided_by": who,
                "reason": reason, "rule": self.rule, "request_id": id, "operation": self.command,
            }),
        });
        hub.publish(ServerEvent::ToolResult {
            session: session.into(),
            item,
            ok: true,
            output: format!("{who} · accept · {reason} ({})", self.rule),
        });
        hub.publish(ServerEvent::ApprovalResolved {
            request_id: id.into(),
            decision: "accept".into(),
            automatic: true,
        });
    }
}

pub(crate) fn check(scope: &Scope, input: &Value, protected: &[PathBuf]) -> Option<ApprovedShell> {
    // Extending the tool schema with options such as dangerouslyDisableSandbox
    // must not silently extend the allowlist.
    if !cfg!(target_os = "linux")
        || scope.cwd.canonicalize().ok()? != scope.cwd
        || !input
            .as_object()?
            .iter()
            .all(|(key, value)| match key.as_str() {
                "command" | "description" => value.is_string(),
                "timeout" => value.as_u64().is_some(),
                "run_in_background" | "dangerouslyDisableSandbox" => value == false,
                _ => false,
            })
    {
        return None;
    }
    let protected = ProtectedPaths { state: protected, memory: &scope.memory_roots };
    let command = input["command"].as_str()?;
    let stages = parse(command)?;
    let mut cwd = scope.cwd.as_path().to_path_buf();
    if protected.state.iter().chain(protected.memory).any(|root| cwd.starts_with(root)) {
        return None;
    }
    // A directory-changing command and all its operands are worktree-local.
    let mut roots = vec![scope.cwd.as_path()];
    if stages.first()?.1.first()? != "cd" {
        roots.push(&scope.scratch);
        roots.extend(scope.read_roots.iter().map(PathBuf::as_path));
    }
    let mut script = format!("cd -- {}", shlex::try_quote(cwd.to_str()?).ok()?);
    let mut rules = Vec::new();
    let scratch = scope.scratch.as_path();
    // Tracks whether every operand checked so far is one of the job's own files.
    let own = std::cell::Cell::new(Some(false));
    let mut own_files = true;
    for (index, (join, words)) in stages.iter().enumerate() {
        script.push_str(match join {
            Join::And => " && ",
            Join::Pipe => " | ",
        });
        let (name, args) = words.split_first()?;
        let operands = |names: &[String], kind| {
            names.iter().all(|name| {
                let checked = checked_path(&roots, &cwd, name, protected, scratch, kind);
                if let Some(path) = &checked {
                    if name != "-" {
                        let mine = super::own_file(scratch, path);
                        own.set(own.get().filter(|_| mine).map(|_| true));
                    }
                }
                checked.is_some()
            })
        };
        if !matches!(name.as_str(), "sleep" | "cat" | "head" | "tail" | "wc" | "stat" | "sed" | "grep") {
            own_files = false;
        }

        if name == "cd" {
            if index != 0 || !matches!(stages.get(1), Some((Join::And, _))) {
                return None;
            }
            let [path] = args else { return None };
            cwd = checked_path(&roots, &cwd, path, protected, scratch, PathKind::Directory)?;
            script.push_str("cd -- ");
            script.push_str(&shlex::try_quote(cwd.to_str()?).ok()?);
            rules.push("cd");
            continue;
        }
        let (_, positional) = match name.as_str() {
            "ls" => flags(args, "1aAldFhirtSu", &["--color=never"], "", &[])?,
            "cat" => flags(args, "nbsETAv", &[], "", &[])?,
            "head" | "tail" => flags(args, "qv", &[], "nc", &["--lines", "--bytes"])?,
            "wc" => flags(args, "clmwL", &[], "", &[])?,
            "stat" => flags(args, "tf", &[], "", &[])?,
            _ => (&[][..], args),
        };
        match name.as_str() {
            "ls" => {
                if !operands(positional, PathKind::Entry)
                    || (positional.is_empty()
                        && checked_path(&roots, &cwd, ".", protected, scratch, PathKind::Directory).is_none())
                {
                    return None;
                }
            }
            "cat" | "head" | "tail" | "wc" if operands(positional, PathKind::FileOrStdin) => {}
            "stat" if !positional.is_empty() && operands(positional, PathKind::Entry) => {}
            "pwd" if args.is_empty() => {}
            "mkdir" => {
                let (_, paths) = flags(args, "pv", &["--parents", "--verbose"], "", &[])?;
                if paths.is_empty() || !paths.iter().all(|path| {
                    new_directory(&[&scope.cwd, &scope.scratch], &cwd, path, protected, scratch)
                }) {
                    return None;
                }
            }
            "sleep" if matches!(args, [duration] if seconds(duration)) => {}
            "date" if args.iter().all(|arg| arg == "-u" || arg.starts_with('+')) => {}
            "which" if !args.is_empty() && args.iter().all(|arg| is_identifier(arg)) => {}
            "ps" if ps_args(args) => {}
            "pgrep" if !flags(args, "aflix", &[], "", &[])?.1.is_empty() => {}
            "sed" => {
                let [quiet, script, paths @ ..] = args else {
                    return None;
                };
                if quiet != "-n" || !sed_print(script) || !operands(paths, PathKind::FileOrStdin) {
                    return None;
                }
            }
            "grep" | "rg" => {
                let (options, positional) = if name == "rg" {
                    flags(
                        args,
                        "nHhilvowxFcSqUI",
                        &["--files", "--no-ignore", "--hidden", "--color=never"],
                        "mABC",
                        &[],
                    )?
                } else {
                    flags(args, "EFGHhIiLlnovwxcq", &["--color=never"], "mABC", &[])?
                };
                let files = options.iter().any(|option| option == "--files");
                let paths = if files {
                    positional
                } else {
                    positional.get(1..)?
                };
                // Directory listing must skip ignore-file reads. Content searches
                // use individual regular files so each can be checked, including
                // its link count. Pipeline input has already passed an earlier stage.
                let kind = if files {
                    PathKind::Search
                } else {
                    PathKind::FileOrStdin
                };
                if !operands(paths, kind)
                    || (paths.is_empty()
                        && name == "rg"
                        && *join != Join::Pipe
                        && (!files
                            || checked_path(&roots, &cwd, ".", protected, scratch, PathKind::Search)
                                .is_none()))
                    || (files && !options.iter().any(|option| option == "--no-ignore"))
                {
                    return None;
                }
            }
            "find" => {
                if !find_args(&roots, &cwd, args, protected, scratch) {
                    return None;
                }
            }
            "jq" => {
                let (_, operands_and_filter) = flags(args, "crSMs", &[], "", &[])?;
                let (filter, paths) = operands_and_filter.split_first()?;
                if !jq_filter(filter) || !operands(paths, PathKind::FileOrStdin) {
                    return None;
                }
            }
            _ => return None,
        }
        let executable = Path::new("/usr/bin").join(name);
        if !executable.is_file() {
            return None;
        }
        script.push_str(executable.to_str()?);
        if name == "rg" {
            script.push_str(" --no-config");
        }
        if name == "sed" {
            script.push_str(" --sandbox");
        }
        for arg in args {
            script.push(' ');
            script.push_str(&shlex::try_quote(arg).ok()?);
        }
        rules.push(if name == "sed" {
            "sed -n"
        } else {
            name.as_str()
        });
    }
    // Preserve shell operators while preventing aliases, project PATH entries,
    // shell startup files, and tool configuration from changing the checked commands.
    let mut updated = input.clone();
    updated["command"] = json!(format!(
        "/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C /bin/bash --noprofile --norc -c {}",
        shlex::try_quote(&script).ok()?,
    ));
    let first = *rules.first()?;
    let basic = rules.iter().all(|rule| BASIC_READS.contains(rule));
    // The front desk passes its project as the scratch; only a separate job directory counts.
    let own_files = own_files && own.get() == Some(true) && scope.scratch != scope.cwd;
    let creates_directories = rules.contains(&"mkdir") && rules.iter().all(|rule| matches!(*rule, "mkdir" | "cd"));
    Some(ApprovedShell {
        basic,
        own_files,
        creates_directories,
        input: updated,
        // A leading cd can be saved as an exact command only. Every stage
        // must pass the same checks regardless of which rule ultimately matches.
        prefix: SAVABLE_PREFIXES
            .iter()
            .copied()
            .find(|&prefix| prefix == first),
        rule: rules.join(" + "),
        command: command.into(),
    })
}

fn parse(command: &str) -> Option<Vec<(Join, Vec<String>)>> {
    if command.len() > 8192 || command.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    let mut stages = Vec::new();
    let mut join = Join::And;
    let mut start = 0;
    let mut quote = None;
    let mut chars = command.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '$' | '`') => return None,
            (Some('"'), '\\') => {
                chars.next()?;
            }
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '|' | '&') => {
                stages.push((join, shlex::split(&command[start..index])?));
                join = if c == '&' {
                    chars.next_if(|(_, next)| *next == '&')?;
                    start = index + 2;
                    Join::And
                } else {
                    start = index + 1;
                    Join::Pipe
                };
            }
            (
                None,
                ';' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']' | '{' | '}' | '#' | '!' | '~'
                | '\\',
            ) => return None,
            (None, _) => {}
        }
    }
    stages.push((join, shlex::split(&command[start..])?));
    (stages.len() <= 12 && stages.iter().all(|(_, words)| !words.is_empty())).then_some(stages)
}

#[derive(Clone, Copy)]
struct ProtectedPaths<'a> {
    state: &'a [PathBuf],
    memory: &'a [PathBuf],
}

#[derive(Clone, Copy)]
enum PathKind {
    Entry,
    Directory,
    FileOrStdin,
    Search,
}

fn checked_path(
    roots: &[&Path],
    cwd: &Path,
    name: &str,
    protected: ProtectedPaths<'_>,
    scratch: &Path,
    kind: PathKind,
) -> Option<PathBuf> {
    if name == "-" && matches!(kind, PathKind::FileOrStdin) {
        return Some(PathBuf::from(name));
    }
    if name.is_empty() || name.starts_with(['~', '-']) {
        return None;
    }
    let path = Path::new(name);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    let absolute = cwd.join(path);
    let canonical = absolute.canonicalize().ok()?;
    if !absolute.components().eq(canonical.components())
        || !roots.iter().any(|root| canonical.starts_with(root))
        || protected.memory.iter().any(|root| {
            canonical.starts_with(root)
                || matches!(kind, PathKind::Search) && root.starts_with(&canonical)
        })
        || protected.state.iter().any(|root| {
            // The job directory lives under the server state directory. Only its
            // own files are exempt, never a protected root nested inside it.
            let own = scratch != root.as_path()
                && scratch.starts_with(root)
                && super::own_file(scratch, &canonical);
            (canonical.starts_with(root) && !own) || root.starts_with(&canonical)
        })
    {
        return None;
    }
    let metadata = canonical.metadata().ok()?;
    if metadata.is_dir() && !matches!(kind, PathKind::FileOrStdin) {
        return Some(canonical);
    }
    if !metadata.is_file() || matches!(kind, PathKind::Directory) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return None;
        }
    }
    Some(canonical)
}

/// Whether `name` is a directory below one of `roots` that can be created, or
/// already exists, without passing a link, Git metadata or a protected root.
fn new_directory(roots: &[&Path], cwd: &Path, name: &str, protected: ProtectedPaths<'_>, scratch: &Path) -> bool {
    if name.is_empty() || name.starts_with(['~', '-']) {
        return false;
    }
    let path = Path::new(name);
    if path.components().any(|part| matches!(part, Component::ParentDir | Component::Prefix(_))) {
        return false;
    }
    let absolute: PathBuf = cwd.join(path).components().collect();
    let existing = absolute.ancestors().find(|path| path.symlink_metadata().is_ok());
    roots.iter().any(|root| absolute.strip_prefix(root).is_ok_and(|relative| {
        !relative.iter().any(|part| part.eq_ignore_ascii_case(".git"))
    })) && existing.is_some_and(|path| {
        path.is_dir() && path.canonicalize().is_ok_and(|canonical| canonical.components().eq(path.components()))
    }) && !protected.memory.iter().any(|root| absolute.starts_with(root))
        && !protected.state.iter().any(|root| {
            let own = scratch != root.as_path() && scratch.starts_with(root) && absolute.starts_with(scratch)
                && super::own_file(scratch, &absolute);
            absolute.starts_with(root) && !own
        })
}

// Options must precede operands. A second dash-prefixed operand can otherwise
// be permuted into an option by GNU programs, so reject it unless -- ended parsing.
fn flags<'a>(
    args: &'a [String],
    switches: &str,
    long: &[&str],
    numeric: &str,
    long_numeric: &[&str],
) -> Option<(&'a [String], &'a [String])> {
    let mut index = 0;
    while let Some(arg) = args
        .get(index)
        .filter(|arg| arg.starts_with('-') && arg.as_str() != "-")
    {
        if arg == "--" {
            return Some((&args[..index], &args[index + 1..]));
        }
        if long.contains(&arg.as_str()) {
            index += 1;
            continue;
        }
        let short = arg.strip_prefix('-')?;
        if short.chars().all(|c| switches.contains(c)) {
            index += 1;
            continue;
        }
        if let Some((key, value)) = arg.split_once('=') {
            if !long_numeric.contains(&key) || !integer(value) {
                return None;
            }
        } else if long_numeric.contains(&arg.as_str())
            || (short.len() == 1 && numeric.contains(short))
        {
            index += 1;
            if !integer(args.get(index)?) {
                return None;
            }
        } else if !short.starts_with('-')
            && numeric.contains(short.chars().next()?)
            && integer(&short[1..])
        {
            // A numeric argument attached to its short option, e.g. -n20.
        } else {
            return None;
        }
        index += 1;
    }
    args[index..]
        .iter()
        .all(|arg| !arg.starts_with('-') || arg == "-")
        .then_some((&args[..index], &args[index..]))
}

/// A plain `sleep` duration such as `60`, `1.5`, or `2m`.
fn seconds(value: &str) -> bool {
    let number = value.strip_suffix(['s', 'm']).unwrap_or(value);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, "0"));
    !whole.is_empty() && !fraction.is_empty() && whole.len() <= 6
        && (whole.bytes().chain(fraction.bytes())).all(|byte| byte.is_ascii_digit())
}

fn integer(value: &str) -> bool {
    let value = value.strip_prefix(['+', '-']).unwrap_or(value);
    !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit())
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        && !value.starts_with('-')
}

fn sed_print(script: &str) -> bool {
    let Some(address) = script.strip_suffix('p') else {
        return false;
    };
    address.is_empty()
        || address.split(',').count() <= 2
            && address
                .split(',')
                .all(|n| n == "$" || (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

fn ps_args(args: &[String]) -> bool {
    match args {
        [] => true,
        [arg] => matches!(arg.as_str(), "-e" | "-ef" | "aux" | "-A" | "-f"),
        [arg, columns] if matches!(arg.as_str(), "-eo" | "-o") => {
            columns.split(',').all(is_identifier)
        }
        _ => false,
    }
}

fn jq_filter(filter: &str) -> bool {
    filter == "."
        || filter.strip_prefix('.').is_some_and(|fields| {
            fields.split('.').all(|fields| {
                let name = fields.strip_suffix("[]").unwrap_or(fields);
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
        })
}

fn find_args(roots: &[&Path], cwd: &Path, args: &[String], protected: ProtectedPaths<'_>, scratch: &Path) -> bool {
    let Some((path, expressions)) = args.split_first() else {
        return false;
    };
    // GNU find treats an operand that starts with ! or ( as an expression.
    if path.starts_with(['!', '('])
        || !checked_path(roots, cwd, path, protected, scratch, PathKind::Search).is_some_and(|path| path.is_dir())
    {
        return false;
    }
    let mut expressions = expressions.iter();
    while let Some(expression) = expressions.next() {
        let allowed = match expression.as_str() {
            "-print" => true,
            "-maxdepth" | "-mindepth" => expressions
                .next()
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())),
            "-name" | "-iname" | "-path" => expressions.next().is_some(),
            "-type" => expressions
                .next()
                .is_some_and(|kind| matches!(kind.as_str(), "f" | "d" | "l")),
            _ => false,
        };
        if !allowed {
            return false;
        }
    }
    true
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn leading_cd_checks_every_stage_inside_the_worktree() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let base = base.path().canonicalize()?;
        let worktree = base.join("worktree");
        let scratch = base.join("scratch");
        let references = base.join("references");
        for dir in [
            worktree.join("docs"),
            worktree.join("crates/secretaire/src"),
            scratch.clone(),
            references.clone(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        for path in ["docs/book-harness.md", "crates/secretaire/src/lib.rs"] {
            std::fs::write(worktree.join(path), "foo\nsecond line\n")?;
        }
        std::fs::write(references.join("notes.md"), "reference\n")?;
        std::os::unix::fs::symlink(&references, worktree.join("escape"))?;
        std::os::unix::fs::symlink(worktree.join("docs"), worktree.join("alias"))?;
        std::os::unix::fs::symlink(
            references.join("notes.md"),
            worktree.join("docs/escape.md"),
        )?;
        let scope = Scope {
            cwd: worktree.clone(),
            scratch: scratch.clone(),
            read_roots: vec![references.clone(), PathBuf::from("/etc")],
            memory_roots: vec![],
        };
        let checked = |command: &str| check(&scope, &json!({"command": command}), &[]);
        let absolute = |path: &Path| {
            shlex::try_quote(path.to_str().unwrap()).unwrap().into_owned()
        };
        let absolute_ls = format!("cd {} && ls -la", absolute(&worktree.join("docs")));

        for command in [
            "cd docs && sed -n '1,5p' book-harness.md",
            "cd docs && grep -n foo book-harness.md",
            &absolute_ls,
            "cd docs && cat book-harness.md | head -n1 && wc -l book-harness.md",
            "cd docs && find . -maxdepth 1 -type f",
        ] {
            let approved = checked(command).unwrap_or_else(|| panic!("expected allow: {command}"));
            assert_eq!(approved.prefix, None, "{command}");
        }
        if Path::new("/usr/bin/rg").is_file() {
            let approved = checked("cd crates && rg -n foo secretaire/src/lib.rs").unwrap();
            assert_eq!(approved.prefix, None);
            assert!(checked("cd crates && rg --files --no-ignore secretaire/src").is_some());
        }
        let approved = checked("cd docs && cat book-harness.md | head -n1").unwrap();
        let output = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(approved.input["command"].as_str().unwrap())
            .current_dir(&references)
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"foo\n");
        assert_eq!(approved.rule, "cd + cat + head");

        let external_cd = format!("cd {} && cat notes.md", absolute(&references));
        let external_read = format!(
            "cd docs && cat {}",
            absolute(&references.join("notes.md"))
        );
        let external_find = format!("cd docs && find {} -print", absolute(&references));
        let scratch_cd = format!("cd {} && ls", absolute(&scratch));
        for command in [
            &external_cd,
            &external_read,
            &external_find,
            &scratch_cd,
            "cd /etc && cat passwd",
            "cd .. && ls",
            "cd docs/.. && ls",
            "cd docs && rm -rf book-harness.md",
            "cd docs && cat book-harness.md | head && rm -rf book-harness.md",
            "cd docs; cat book-harness.md",
            "cd docs | cat book-harness.md",
            "cd docs || ls",
            "cd $(foo) && ls",
            "cd `foo` && ls",
            "(cd docs && ls)",
            "cd $DIR && ls",
            "cd doc* && ls",
            "cd - && ls",
            "cd && ls",
            "cd -",
            "cd",
            "cd docs",
            "cd docs &&",
            "pwd && cd docs && ls",
            "cd docs && cd . && ls",
            "cd escape && ls",
            "cd alias && ls",
            "cd docs && cat escape.md",
            "cd docs && cat notes.md",
            "cd docs && cat ../docs/book-harness.md",
            "cd crates && rg -n foo secretaire/src",
        ] {
            assert!(checked(command).is_none(), "expected review: {command}");
        }
        assert!(check(
            &scope,
            &json!({"command": "cd docs && ls"}),
            &[worktree.join("docs")],
        )
        .is_none());
        Ok(())
    }

    #[test]
    fn command_and_operand_allowlist_fails_closed() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let make_dir = |name| -> anyhow::Result<PathBuf> {
            let path = base.path().join(name);
            std::fs::create_dir(&path)?;
            Ok(path.canonicalize()?)
        };
        let cwd = make_dir("worktree")?;
        let scratch = make_dir("scratch")?;
        let read_root = make_dir("references")?;
        let protected = make_dir("protected")?;
        let scope = Scope {
            cwd,
            scratch,
            read_roots: vec![read_root.clone(), protected.clone()],
            memory_roots: vec![],
        };
        for dir in [&scope.cwd, &scope.scratch, &read_root, &protected] {
            std::fs::write(dir.join("notes.md"), "first line\nnext line\n")?;
        }
        std::fs::create_dir(scope.cwd.join("sub dir"))?;
        std::os::unix::fs::symlink(&protected, scope.cwd.join("escape"))?;
        std::os::unix::fs::symlink(scope.cwd.join("notes.md"), scope.cwd.join("symlink"))?;
        std::fs::hard_link(protected.join("notes.md"), scope.cwd.join("hardlink"))?;
        let checked = |command: &str| {
            check(
                &scope,
                &json!({"command": command}),
                std::slice::from_ref(&protected),
            )
        };

        let reads = [
            "pwd",
            "ls -la",
            "ls --color=never .",
            "cat notes.md",
            "head -n1 notes.md",
            "tail -n +1 notes.md",
            "stat notes.md",
            "wc -l notes.md",
            "cat notes.md | head -n 1 && wc -l notes.md",
            "cd 'sub dir' && pwd",
            "cat notes.md | grep -Ei 'name(/a|/b)'",
            "ps -eo pid,lstart,args | grep -Ei '/absolute/pattern|other'",
            "find . -maxdepth 2 -iname '*.md' -print",
            "sed -n '1,2p' notes.md",
            "grep -n -- '-literal' notes.md",
            "which cargo",
            "date -u +%F",
        ];
        for command in reads {
            let allowed = checked(command).unwrap_or_else(|| panic!("expected allow: {command}"));
            assert_ne!(allowed.input["command"], command);
        }
        assert_eq!(checked("sed -n '1,2p' notes.md").unwrap().prefix, Some("sed -n"));
        assert_eq!(checked("cd 'sub dir' && pwd").unwrap().prefix, None);
        for (tool, sample) in [
            ("rg", "rg -n line notes.md"),
            ("rg", "rg --files --no-ignore"),
            ("jq", "jq -r '.results[]' notes.md"),
        ] {
            if Path::new("/usr/bin").join(tool).is_file() {
                assert_eq!(checked(sample).unwrap().prefix, Some(tool), "{sample}");
            }
        }
        for dir in [&read_root, &scope.scratch] {
            let command = format!(
                "cat {}",
                shlex::try_quote(dir.join("notes.md").to_str().unwrap())?
            );
            assert!(checked(&command).is_some());
        }

        let rejected = [
            "cat $(pwd)",
            "cat `pwd`",
            "cat \"$(pwd)\"",
            "cat <(pwd)",
            "cat >(pwd)",
            "pwd; pwd",
            "pwd || pwd",
            "pwd &",
            "pwd |",
            "pwd |& pwd",
            "pwd &&",
            "(pwd)",
            "pwd\npwd",
            "pwd > out",
            "pwd > /etc/out",
            "cat < notes.md",
            "cat <<EOF",
            "cat <<< EOF",
            "PATH=. cat notes.md",
            "GIT_OPTIONAL_LOCKS=0 git status",
            "./cat notes.md",
            "cd subdir | pwd",
            "cat sub/../notes.md",
            "cat ../protected/notes.md",
            "cat escape/notes.md",
            "cat ~/notes.md",
            "cat \"$HOME/notes.md\"",
            "cat \"$TMPDIR/notes.md\"",
            "cat escape/*",
            "cat - notes.md --help",
            "cat symlink",
            "cat hardlink",
            "cat missing",
            "ls -R .",
            "rg --pre sh notes.md",
            "rg needle .",
            "grep -f notes.md notes.md",
            "find -L . -print",
            "find . -exec pwd '{}' +",
            "find . -delete",
            "find . -fprint out",
            "sed -n '1e id' notes.md",
            "sed -n '1w out' notes.md",
            "sed -n '1r /etc/passwd' notes.md",
            "jq 'include \"external\"; .' notes.md",
            "date --set=now",
            "sh -c 'pwd'",
            "eval pwd",
            "git status",
            "git diff",
            "git commit -m message",
            "git push",
            "cargo test --offline",
            "gradle test",
            "npm test",
            "python3 -I -c 'print(1)'",
            "curl example.invalid",
            "rm -rf out",
            "gh issue edit 1",
            "systemctl enable demo",
            "systemctl disable demo",
            "systemctl restart demo",
            "secretaire --swap",
            "cargo publish",
            "gpg --sign notes.md",
        ];
        for command in rejected {
            assert!(checked(command).is_none(), "expected review: {command}");
        }
        let command = format!("cat {}/notes.md", protected.display());
        assert!(checked(&command).is_none());
        assert!(check(
            &scope,
            &json!({"command":"pwd","dangerouslyDisableSandbox":true}),
            &[]
        )
        .is_none());
        assert!(check(&scope, &json!({"command":"pwd","unexpected":"value"}), &[]).is_none());

        let approved = checked("cat notes.md | head -n 1").unwrap();
        let output = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(approved.input["command"].as_str().unwrap())
            .current_dir(&protected)
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"first line\n");
        assert_eq!(approved.rule, "cat + head");
        Ok(())
    }
}
