//! The programs and arguments that need a person.

use std::path::PathBuf;

use super::parse::{self, Simple, Word, UNKNOWN};
use super::Reason::{self, Administrator, Credentials, DestructiveGit, ExternalWrite, Unclassified};

/// Scripts and wrappers nested deeper than this ask a person.
const DEPTH: usize = 8;

pub(super) struct Check<'a> {
    pub protected: &'a [PathBuf],
    pub home: Option<String>,
}

/// The options a wrapper accepts. Anything else is not understood.
struct Options {
    flags: &'static str,
    values: &'static str,
    long_flags: &'static [&'static str],
    long_values: &'static [&'static str],
}

impl Options {
    const NONE: Self = Self { flags: "", values: "", long_flags: &[], long_values: &[] };

    /// The words from the first operand on, and the short options with values.
    fn read<'w>(&self, args: &'w [Word]) -> Option<(&'w [Word], Vec<(char, Word)>)> {
        let mut seen = Vec::new();
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            let text = arg.text.as_str();
            if text == "--" {
                index += 1;
                break;
            }
            if !text.starts_with('-') || text == "-" {
                break;
            }
            if arg.glob {
                return None;
            }
            index += 1;
            if let Some(long) = text.strip_prefix("--") {
                let (name, attached) = long.split_once('=').map_or((long, false), |(name, _)| (name, true));
                if self.long_values.contains(&name) {
                    index += usize::from(!attached);
                } else if !self.long_flags.contains(&name) || arg.dynamic {
                    return None;
                }
                continue;
            }
            for (position, letter) in text.char_indices().skip(1) {
                if self.values.contains(letter) {
                    let rest = &text[position + letter.len_utf8()..];
                    let value = if rest.is_empty() {
                        index += 1;
                        args.get(index - 1)?.clone()
                    } else {
                        Word { text: rest.into(), ..arg.clone() }
                    };
                    seen.push((letter, value));
                    break;
                }
                if !self.flags.contains(letter) {
                    return None;
                }
                seen.push((letter, Word::default()));
            }
        }
        Some((args.get(index..).unwrap_or_default(), seen))
    }
}

fn name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether `arg` spells the long option, possibly abbreviated or with a value.
fn long(arg: &str, option: &str) -> bool {
    let arg = arg.split('=').next().unwrap_or_default();
    arg.len() > 3 && option.starts_with(arg)
}

impl Check<'_> {
    pub(super) fn script(&self, text: &str, depth: usize) -> Option<Reason> {
        if depth > DEPTH {
            return Some(Unclassified);
        }
        let Some(script) = parse::script(text, self.home.as_deref()) else {
            return Some(Unclassified);
        };
        self.paths(&script.words).or_else(|| {
            let found: Vec<_> = script.commands.iter().filter_map(|command| self.invocation(command, depth)).collect();
            // Name a definite operation before an unreadable one.
            found.iter().copied().find(|reason| *reason != Unclassified).or_else(|| found.first().copied())
        })
    }

    pub(super) fn paths(&self, words: &[Word]) -> Option<Reason> {
        words.iter().any(|word| {
            word.text.split('=').any(|part| (part.contains('/') || part.starts_with('.') || part.starts_with('~'))
                && !part.contains(char::is_whitespace)
                && super::credential(part.trim_start_matches('@'), self.protected))
        }).then_some(Credentials)
    }

    pub(super) fn invocation(&self, command: &Simple, depth: usize) -> Option<Reason> {
        if depth > DEPTH {
            return Some(Unclassified);
        }
        let (word, args) = command.words.split_first()?;
        if word.dynamic || word.glob {
            return Some(Unclassified);
        }
        // Run the words that a wrapper hands on.
        let inner = |words: &[Word]| self.invocation(&Simple {
            words: words.to_vec(),
            assignments: command.assignments.clone(),
            stdin: command.stdin.clone(),
        }, depth + 1);
        let wrapped = |options: Options| match options.read(args) {
            Some((rest, _)) => inner(rest),
            None => Some(Unclassified),
        };
        match name(&word.text) {
            "env" => {
                let options = Options {
                    flags: "i0v", values: "uC", long_flags: &["ignore-environment", "null", "debug"],
                    long_values: &["unset", "chdir"],
                };
                let Some((mut rest, _)) = options.read(args) else { return Some(Unclassified) };
                if rest.first().is_some_and(|word| word.text == "-") {
                    rest = &rest[1..];
                }
                let mut assignments = command.assignments.clone();
                while let Some(first) = rest.first().filter(|word| {
                    word.text.split_once('=').is_some_and(|(key, _)| !key.contains(UNKNOWN))
                }) {
                    assignments.push(first.clone());
                    rest = &rest[1..];
                }
                self.invocation(&Simple { words: rest.to_vec(), assignments, stdin: command.stdin.clone() }, depth + 1)
            }
            "timeout" => {
                let options = Options {
                    flags: "v", values: "sk", long_flags: &["preserve-status", "foreground", "verbose"],
                    long_values: &["signal", "kill-after"],
                };
                match options.read(args) {
                    Some(([_duration, rest @ ..], _)) => inner(rest),
                    Some(([], _)) => None,
                    None => Some(Unclassified),
                }
            }
            "nice" => {
                let numeric = args.first().is_some_and(|arg| {
                    arg.text.len() > 1 && arg.text.starts_with('-') && arg.text[1..].chars().all(|c| c.is_ascii_digit())
                });
                let options = Options { values: "n", long_values: &["adjustment"], ..Options::NONE };
                match options.read(&args[usize::from(numeric)..]) {
                    Some((rest, _)) => inner(rest),
                    None => Some(Unclassified),
                }
            }
            "ionice" => wrapped(Options {
                flags: "t", values: "cn", long_values: &["class", "classdata"], ..Options::NONE
            }),
            "nohup" | "builtin" => wrapped(Options::NONE),
            "setsid" => wrapped(Options { flags: "fwc", long_flags: &["fork", "wait", "ctty"], ..Options::NONE }),
            "stdbuf" => wrapped(Options { values: "ioe", long_values: &["input", "output", "error"], ..Options::NONE }),
            "time" => wrapped(Options {
                flags: "pva", values: "of", long_flags: &["portability", "verbose", "append"],
                long_values: &["output", "format"],
            }),
            "command" => match (Options { flags: "pvV", ..Options::NONE }).read(args) {
                // -v and -V only look the name up.
                Some((_, seen)) if seen.iter().any(|(letter, _)| matches!(letter, 'v' | 'V')) => None,
                Some((rest, _)) => inner(rest),
                None => Some(Unclassified),
            },
            "exec" => wrapped(Options { flags: "cl", values: "a", ..Options::NONE }),
            "flock" => {
                let options = Options {
                    flags: "sxeunFo", values: "wE",
                    long_flags: &["shared", "exclusive", "unlock", "nonblock", "no-fork", "close", "verbose"],
                    long_values: &["timeout", "wait", "conflict-exit-code"],
                };
                match options.read(args) {
                    Some(([_file, flag, rest @ ..], _)) if matches!(flag.text.as_str(), "-c" | "--command") => {
                        match rest.first() {
                            Some(script) if !script.dynamic => self.script(&script.text, depth + 1),
                            _ => Some(Unclassified),
                        }
                    }
                    Some(([_file, rest @ ..], _)) => inner(rest),
                    Some(([], _)) => None,
                    None => Some(Unclassified),
                }
            }
            "xargs" => {
                let options = Options {
                    flags: "0rtpoxi", values: "nPIEsLda",
                    long_flags: &["null", "no-run-if-empty", "verbose", "interactive", "open-tty", "exit"],
                    long_values: &["max-args", "max-procs", "max-lines", "delimiter", "arg-file", "max-chars"],
                };
                let Some((rest, seen)) = options.read(args) else { return Some(Unclassified) };
                if rest.is_empty() {
                    return None;
                }
                let replace = seen.iter().find_map(|(letter, value)| match letter {
                    'I' => Some(value.text.clone()),
                    'i' => Some("{}".into()),
                    _ => None,
                });
                let mut words = rest.to_vec();
                match replace {
                    Some(mark) => for word in &mut words {
                        if !mark.is_empty() && word.text.contains(&mark) {
                            word.text = word.text.replace(&mark, &UNKNOWN.to_string());
                            word.dynamic = true;
                        }
                    },
                    // Standard input supplies more arguments.
                    None => words.push(Word::unknown()),
                }
                self.invocation(&Simple { words, assignments: command.assignments.clone(), stdin: None }, depth + 1)
            }
            "npx" | "bunx" | "uvx" | "pipx" => {
                let options = Options {
                    flags: "yq", values: "p", long_flags: &["yes", "no", "quiet", "no-install"],
                    long_values: &["package", "from", "spec"],
                };
                let args = match args.split_first() {
                    Some((first, rest)) if name(&word.text) == "pipx" && first.text == "run" => rest,
                    _ => args,
                };
                match options.read(args) {
                    Some((rest, _)) => inner(rest),
                    None => Some(Unclassified),
                }
            }
            "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish" => self.shell(command, args, depth),
            "eval" => {
                if args.iter().any(|arg| arg.dynamic || arg.glob) {
                    return Some(Unclassified);
                }
                let text: Vec<_> = args.iter().map(|arg| arg.text.as_str()).collect();
                self.script(&text.join(" "), depth + 1)
            }
            "trap" => {
                let args = match args.split_first() {
                    Some((first, rest)) if first.text == "--" => rest,
                    _ => args,
                };
                match args.first() {
                    Some(action) if action.dynamic => Some(Unclassified),
                    Some(action) if args.len() > 1 && !action.text.starts_with('-') => {
                        self.script(&action.text, depth + 1)
                    }
                    _ => None,
                }
            }
            "source" | "." => args.first().is_some_and(|file| file.dynamic).then_some(Unclassified),
            "alias" => args.iter().any(|arg| arg.dynamic || arg.text.contains('=')).then_some(Unclassified),
            "mapfile" | "readarray" => args.iter().any(|arg| arg.text.starts_with("-C")).then_some(Unclassified),
            "find" => self.find(command, args, depth),
            "fd" | "fdfind" => match args.iter().position(|arg| {
                matches!(arg.text.as_str(), "-x" | "-X" | "--exec" | "--exec-batch")
            }) {
                Some(index) => {
                    let mut words = args[index + 1..].to_vec();
                    words.push(Word::unknown());
                    inner(&words)
                }
                None => None,
            },
            "rg" => args.iter().any(|arg| arg.text.starts_with("--pre=") || arg.text == "--pre"
                || arg.text.starts_with("--hostname-bin")).then_some(Unclassified),
            "sudo" | "doas" | "su" | "pkexec" | "run0" | "runuser" => Some(Administrator),
            // These start other programs in ways that are not followed.
            "watch" | "parallel" | "strace" | "ltrace" | "gdb" | "lldb" | "unshare" | "nsenter" | "chroot"
            | "systemd-run" | "bwrap" | "firejail" | "at" | "batch" | "crontab" | "script" | "expect" | "coproc"
            | "taskset" | "chrt" | "prlimit" | "setpriv" | "capsh" | "fakeroot" | "run-parts" | "start-stop-daemon"
            | "busybox" | "toybox" | "sshpass" | "proxychains" | "proxychains4" | "torsocks" | "tmux" | "screen"
            | "chronic" | "unbuffer" | "enable" | "cmd.exe" | "wsl.exe" | "osascript" => Some(Unclassified),
            "powershell.exe" | "pwsh" => Some(if args.iter().any(|arg| arg.text.eq_ignore_ascii_case("runas")) {
                Administrator
            } else {
                Unclassified
            }),
            "git" => self.git(command, args, depth),
            "curl" => curl(command, args),
            program => program_rule(program, args, self.home.as_deref()),
        }
    }

    fn shell(&self, command: &Simple, args: &[Word], depth: usize) -> Option<Reason> {
        let fish = command.words.first().is_some_and(|word| name(&word.text) == "fish");
        let (mut script, mut stdin) = (false, false);
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            let text = arg.text.as_str();
            if text == "--" || text == "-" {
                index += 1;
                break;
            }
            if !(text.starts_with('-') || text.starts_with('+')) {
                break;
            }
            if arg.dynamic || arg.glob {
                return Some(Unclassified);
            }
            index += 1;
            match text {
                "--version" | "--help" => return None,
                "--rcfile" | "--init-file" => index += 1,
                "--noprofile" | "--norc" | "--posix" | "--login" | "--verbose" | "--restricted" | "--noediting" => {}
                _ if text.starts_with("--") => return Some(Unclassified),
                _ => for letter in text.chars().skip(1) {
                    match letter {
                        'c' => script = true,
                        's' => stdin = true,
                        'o' | 'O' => index += 1,
                        // fish runs the value of -C.
                        'C' if fish => return Some(Unclassified),
                        letter if letter.is_ascii_alphabetic() => {}
                        _ => return Some(Unclassified),
                    }
                },
            }
        }
        let operand = args.get(index);
        if script {
            return match operand {
                Some(text) if !text.dynamic && !text.glob => self.script(&text.text, depth + 1),
                _ => Some(Unclassified),
            };
        }
        match operand {
            // A script file runs like any other program.
            Some(file) if !stdin => file.dynamic.then_some(Unclassified),
            _ => match &command.stdin {
                Some(text) => self.script(text, depth + 1),
                None => Some(Unclassified),
            },
        }
    }

    fn find(&self, command: &Simple, args: &[Word], depth: usize) -> Option<Reason> {
        if args.iter().any(|arg| arg.text == "-delete") && args.iter().any(|arg| arg.text.contains(".git")) {
            return Some(DestructiveGit);
        }
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            index += 1;
            if !matches!(arg.text.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                continue;
            }
            let mut words = Vec::new();
            while let Some(word) = args.get(index).filter(|word| !matches!(word.text.as_str(), ";" | "+")) {
                let mut word = word.clone();
                if word.text.contains("{}") {
                    word.text = word.text.replace("{}", &UNKNOWN.to_string());
                    word.dynamic = true;
                }
                words.push(word);
                index += 1;
            }
            let found = self.invocation(&Simple { words, assignments: command.assignments.clone(), stdin: None }, depth + 1);
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn git(&self, command: &Simple, args: &[Word], depth: usize) -> Option<Reason> {
        let mut index = 0;
        let mut pull_rebase = false;
        while let Some(arg) = args.get(index).filter(|arg| arg.text.starts_with('-')) {
            index += 1;
            match arg.text.as_str() {
                "-C" | "--git-dir" | "--work-tree" | "--namespace" => index += 1,
                "-c" => {
                    let setting = args.get(index)?;
                    index += 1;
                    let (key, value) = setting.text.split_once('=').unwrap_or((&setting.text, "true"));
                    let key = key.to_ascii_lowercase();
                    if key.contains(UNKNOWN) || key.starts_with("alias.") || key == "core.sshcommand" {
                        return Some(Unclassified);
                    }
                    pull_rebase |= key == "pull.rebase" && value != "false";
                }
                "--no-pager" | "-p" | "-P" | "--paginate" | "--bare" | "--no-replace-objects" | "--literal-pathspecs"
                | "--glob-pathspecs" | "--noglob-pathspecs" | "--icase-pathspecs" | "--no-optional-locks"
                | "--version" | "--help" | "-h" | "-v" | "--html-path" | "--exec-path" | "--man-path" | "--info-path" => {}
                text if ["--git-dir=", "--work-tree=", "--namespace="].iter().any(|option| text.starts_with(option)) => {}
                _ => return Some(Unclassified),
            }
        }
        let (sub, args) = args.get(index..)?.split_first()?;
        if sub.dynamic || sub.glob {
            return Some(Unclassified);
        }
        let sub = sub.text.as_str();
        let has = |names: &[&str]| args.iter().any(|arg| names.contains(&arg.text.as_str()));
        let has_long = |names: &[&str]| args.iter().any(|arg| names.iter().any(|name| long(&arg.text, name)));
        let short = |letter: char| args.iter().any(|arg| {
            let arg = &arg.text;
            arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(letter)
        });
        match sub {
            "push" | "send-email" | "send-pack" => return Some(ExternalWrite),
            "lfs" | "subtree" | "svn" if has(&["push", "dcommit"]) => return Some(ExternalWrite),
            "credential" | "credential-store" | "credential-cache" => return Some(Credentials),
            _ => {}
        }
        // A value only known at run time could be a destructive option or a path.
        let sensitive = matches!(sub, "reset" | "clean" | "checkout" | "restore" | "switch" | "branch" | "tag"
            | "commit" | "stash" | "pull" | "reflog" | "gc" | "worktree" | "rm" | "lfs" | "subtree" | "svn" | "config");
        // Whether the word after this option is its value.
        let takes_value = |option: &str| {
            let cluster = option.starts_with('-') && !option.starts_with("--");
            let last = option.chars().next_back().unwrap_or_default();
            matches!(option, "--message" | "--file" | "--author" | "--date" | "--orphan")
                || cluster && (matches!(sub, "commit" | "tag" | "stash") && matches!(last, 'm' | 'F')
                    || matches!(sub, "checkout" | "worktree") && last == 'b' || sub == "switch" && last == 'c')
        };
        let unclear = sensitive && args.iter().enumerate().any(|(index, arg)| {
            let value = index > 0 && takes_value(&args[index - 1].text);
            let attached = ["--message=", "--file=", "--author=", "--date="].iter()
                .any(|option| arg.text.starts_with(option))
                || matches!(sub, "commit" | "tag") && (arg.text.starts_with("-m") || arg.text.starts_with("-am"));
            (arg.dynamic || arg.glob) && !value && !attached
        });
        // Option letters are read only from words that are options, not values.
        let force = short('f') || has_long(&["--force"]);
        let destructive = match sub {
            "reset" => has_long(&["--hard", "--merge", "--keep"]),
            "clean" => force && !short('n') && !has_long(&["--dry-run"]),
            // Only new branches, detaching and `checkout -` cannot overwrite files.
            "checkout" => {
                let creates = has(&["-b", "--orphan", "--detach"]);
                let operands = args.iter().enumerate().filter(|(index, arg)| {
                    !arg.text.starts_with('-') && !(*index > 0 && matches!(args[index - 1].text.as_str(), "-b" | "--orphan"))
                }).count();
                let previous = args.len() == 1 && args[0].text == "-";
                force || short('B') || short('p') || has(&["--", "."])
                    || has_long(&["--patch", "--ours", "--theirs", "--pathspec-from-file", "--merge"])
                    || !previous && operands > usize::from(creates)
            }
            "restore" => !(has_long(&["--staged"]) || short('S')) || has_long(&["--worktree"]) || short('W'),
            "switch" => force || has_long(&["--discard-changes", "--force-create"]) || short('C'),
            "branch" => force || short('D') || short('M') || short('C'),
            "tag" => force || short('d') || has_long(&["--delete"]),
            "rebase" => !has(&["--abort", "--continue", "--skip", "--show-current-patch"]),
            "pull" => force || short('r') || args.iter().any(|arg| {
                long(&arg.text, "--rebase") && !arg.text.ends_with("=false")
            }) || pull_rebase && !has(&["--no-rebase", "--rebase=false", "--ff-only"]),
            "commit" => has_long(&["--amend"]),
            "filter-branch" | "filter-repo" | "replace" | "prune" | "update-ref" => true,
            "reflog" => has(&["expire", "delete"]),
            "gc" => has_long(&["--prune"]),
            "stash" => has(&["drop", "clear"]),
            "worktree" => has(&["remove"]) && force || short('B'),
            "rm" => force,
            "config" if args.iter().any(|arg| arg.text.starts_with("alias."))
                && args.iter().filter(|arg| !arg.text.starts_with('-')).count() > 1 => return Some(Unclassified),
            "difftool" if short('x') || has_long(&["--extcmd"]) => return Some(Unclassified),
            "submodule" => {
                let index = args.iter().position(|arg| arg.text == "foreach")?;
                let rest: Vec<_> = args[index + 1..].iter().filter(|arg| arg.text != "--recursive").collect();
                if rest.iter().any(|arg| arg.dynamic) {
                    return Some(Unclassified);
                }
                let text: Vec<_> = rest.iter().map(|arg| arg.text.as_str()).collect();
                return self.script(&text.join(" "), depth + 1);
            }
            "bisect" => {
                return match args.split_first() {
                    Some((first, _)) if first.dynamic => Some(Unclassified),
                    Some((first, rest)) if first.text == "run" => self.invocation(&Simple {
                        words: rest.to_vec(), assignments: command.assignments.clone(), stdin: None,
                    }, depth + 1),
                    _ => None,
                };
            }
            "add" | "am" | "annotate" | "apply" | "archive" | "blame" | "bundle" | "cat-file" | "check-attr"
            | "check-ignore" | "check-ref-format" | "cherry" | "cherry-pick" | "clone" | "commit-tree" | "config"
            | "count-objects" | "describe" | "diff" | "diff-files" | "diff-index" | "diff-tree" | "difftool" | "fetch"
            | "for-each-ref" | "format-patch" | "fsck" | "grep" | "hash-object" | "help" | "init"
            | "interpret-trailers" | "lfs" | "log" | "ls-files" | "ls-remote" | "ls-tree" | "mailinfo" | "maintenance"
            | "merge" | "merge-base" | "merge-file" | "merge-tree" | "mergetool" | "mktree" | "mv" | "name-rev"
            | "notes" | "patch-id" | "range-diff" | "read-tree" | "remote" | "rev-list" | "rev-parse" | "revert"
            | "shortlog" | "show" | "show-branch" | "show-ref" | "sparse-checkout" | "status" | "subtree" | "svn"
            | "symbolic-ref" | "update-index" | "var" | "verify-commit" | "verify-tag" | "version" | "whatchanged"
            | "write-tree" => false,
            // An unknown name can be an alias for anything.
            _ => return Some(Unclassified),
        };
        if destructive {
            Some(DestructiveGit)
        } else {
            unclear.then_some(Unclassified)
        }
    }
}

const CURL_VALUES: &str = "dFHoXuAebcDTxKmwErYyzCPQtUh";
const CURL_LONG_VALUES: &[&str] = &[
    "header", "output", "user", "user-agent", "referer", "cookie", "cookie-jar", "dump-header", "max-time",
    "connect-timeout", "write-out", "retry", "retry-delay", "retry-max-time", "cacert", "cert", "key",
    "oauth2-bearer", "unix-socket", "range", "limit-rate", "output-dir",
];

fn curl(command: &Simple, args: &[Word]) -> Option<Reason> {
    let method = |value: &Word| value.dynamic
        || ["POST", "PUT", "PATCH", "DELETE"].iter().any(|name| value.text.eq_ignore_ascii_case(name));
    let (mut writes, mut unclear, mut configured) = (false, false, false);
    let mut proxied = command.assignments.iter().any(|assignment| {
        assignment.text.split('=').next().is_some_and(|key| key.to_ascii_lowercase().ends_with("_proxy"))
    });
    let mut urls = Vec::new();
    let mut index = 0;
    let mut options = true;
    while let Some(arg) = args.get(index) {
        index += 1;
        let text = arg.text.as_str();
        if options && text == "--" {
            options = false;
        } else if let Some(long) = text.strip_prefix("--").filter(|_| options) {
            let (option, attached) = match long.split_once('=') {
                Some((option, value)) => (option, Some(Word { text: value.into(), ..arg.clone() })),
                None => (long, None),
            };
            unclear |= option.contains(UNKNOWN);
            let sends = ["data", "form", "upload-file", "json"].iter().any(|prefix| option.starts_with(prefix));
            let redirects = matches!(option, "proxy" | "preproxy" | "connect-to" | "resolve")
                || option.starts_with("socks");
            if sends || redirects || matches!(option, "request" | "config" | "url") || CURL_LONG_VALUES.contains(&option) {
                let value = attached.unwrap_or_else(|| {
                    index += 1;
                    args.get(index - 1).cloned().unwrap_or_default()
                });
                writes |= sends || option == "request" && method(&value);
                proxied |= redirects;
                configured |= option == "config";
                if option == "url" {
                    urls.push(value);
                }
            }
        } else if let Some(cluster) = text.strip_prefix('-').filter(|cluster| options && !cluster.is_empty()) {
            for (position, letter) in cluster.char_indices() {
                unclear |= letter == UNKNOWN;
                if !CURL_VALUES.contains(letter) {
                    continue;
                }
                let rest = &cluster[position + 1..];
                let value = if rest.is_empty() {
                    index += 1;
                    args.get(index - 1).cloned().unwrap_or_default()
                } else {
                    Word { text: rest.into(), ..arg.clone() }
                };
                writes |= matches!(letter, 'd' | 'F' | 'T') || letter == 'X' && method(&value);
                proxied |= letter == 'x';
                configured |= letter == 'K';
                break;
            }
        } else {
            // A value that is wholly unknown could be any option.
            unclear |= text.starts_with(UNKNOWN);
            urls.push(arg.clone());
        }
    }
    // Requests to a server on this machine are how local programs are tested.
    let local = !proxied && !urls.is_empty() && urls.iter().all(|url| {
        let host = url.text.split_once("://").map_or(url.text.as_str(), |(_, rest)| rest);
        let host = host.split(['/', '?', '#']).next().unwrap_or_default();
        !host.contains('@') && (host == "[::1]" || host.starts_with("[::1]:")
            || matches!(host.split(':').next(), Some("localhost" | "127.0.0.1")))
    });
    if configured {
        Some(Unclassified)
    } else if writes && !local {
        Some(ExternalWrite)
    } else {
        unclear.then_some(Unclassified)
    }
}

fn program_rule(program: &str, args: &[Word], home: Option<&str>) -> Option<Reason> {
    let texts: Vec<_> = args.iter().map(|arg| arg.text.as_str()).collect();
    let texts = texts.as_slice();
    let has = |names: &[&str]| texts.iter().any(|arg| names.contains(arg));
    // A short option cluster such as -fdx contains the letter.
    let short = |letter: char| texts.iter().any(|arg| {
        arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg.contains(letter)
    });
    // The subcommand, past global options that take a value.
    let first_word = args.iter().enumerate().find(|(index, arg)| {
        let valued = *index > 0 && matches!(texts[index - 1], "--config" | "-Z" | "--color" | "--manifest-path"
            | "-C" | "--prefix" | "--cwd" | "--dir" | "-f" | "--file" | "--directory" | "--context" | "--namespace"
            | "-n" | "--profile" | "--region" | "--project");
        !valued && !arg.text.starts_with('-') && !(program == "cargo" && arg.text.starts_with('+'))
    }).map(|(_, arg)| arg);
    let first = first_word.map(|word| word.text.as_str()).unwrap_or_default();
    let decided_by_arguments = matches!(program, "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "twine" | "gem" | "poetry"
        | "uv" | "hatch" | "flit" | "docker" | "podman" | "helm" | "vsce" | "ovsx" | "mvn" | "gradle" | "gradlew"
        | "make" | "just" | "vercel" | "netlify" | "firebase" | "fly" | "flyctl" | "wrangler" | "heroku" | "surge"
        | "aws" | "gcloud" | "az" | "gsutil" | "kubectl" | "terraform" | "tofu" | "apt" | "apt-get"
        | "dnf" | "yum" | "zypper" | "apk" | "snap");
    let found = match program {
        "apt" | "apt-get" | "dnf" | "yum" | "zypper" | "apk" | "snap"
            if has(&["install", "remove", "upgrade", "purge", "dist-upgrade", "add", "del"]) => Some(Administrator),
        "pacman" if short('S') || short('R') || short('U') => Some(Administrator),
        "secret-tool" | "pass" => Some(Credentials),
        "gh" | "glab" => github(args),
        "wget" if texts.iter().any(|arg| ["--post-data", "--post-file", "--method", "--body-data", "--body-file"]
            .iter().any(|option| arg.starts_with(option))) => Some(ExternalWrite),
        "wget" | "rsync" if args.iter().any(|arg| arg.text.starts_with(UNKNOWN)) => Some(Unclassified),
        "ssh" | "scp" | "sftp" | "ftp" | "telnet" | "mail" | "mailx" | "sendmail" | "msmtp" | "mutt" | "swaks" => Some(ExternalWrite),
        "rsync" if texts.iter().any(|arg| !arg.starts_with('-') && arg.contains(':')) => Some(ExternalWrite),
        "cargo" if matches!(first, "publish" | "yank" | "owner" | "login" | "release") => Some(ExternalWrite),
        "npm" | "pnpm" | "yarn" | "bun" if has(&["publish", "unpublish", "deprecate", "owner", "login", "adduser",
            "dist-tag", "access", "token", "deploy"]) => Some(ExternalWrite),
        "twine" if has(&["upload"]) => Some(ExternalWrite),
        "python" | "python3" if has(&["twine"]) && has(&["upload"]) => Some(ExternalWrite),
        "gem" if matches!(first, "push" | "yank" | "owner" | "signin") => Some(ExternalWrite),
        "poetry" | "uv" | "hatch" | "flit" if first == "publish" => Some(ExternalWrite),
        "docker" | "podman" | "helm" if has(&["push", "login"]) => Some(ExternalWrite),
        "vsce" | "ovsx" if has(&["publish"]) => Some(ExternalWrite),
        "mvn" | "gradle" | "gradlew" | "make" | "just" if texts.iter().any(|arg| {
            let task = arg.rsplit(':').next().unwrap_or(arg);
            ["publish", "deploy", "release"].iter().any(|word| task.starts_with(word)) && !arg.starts_with('-')
        }) => Some(ExternalWrite),
        "vercel" | "netlify" | "firebase" | "fly" | "flyctl" | "wrangler" | "heroku" | "surge"
            if args.is_empty() || has(&["deploy", "publish", "--prod", "release", "push", "delete", "destroy", "secret", "secrets"]) => Some(ExternalWrite),
        "aws" | "gcloud" | "az" | "gsutil" if !matches!(first, "" | "help" | "version") => Some(ExternalWrite),
        "kubectl" if !matches!(first, "get" | "describe" | "logs" | "version" | "config" | "explain" | "") => Some(ExternalWrite),
        "terraform" | "tofu" if has(&["apply", "destroy", "import"]) => Some(ExternalWrite),
        "ansible-playbook" => Some(ExternalWrite),
        "rm" | "shred" | "mv" if texts.iter().any(|arg| arg.trim_end_matches('/') == ".git"
            || arg.trim_end_matches('/').ends_with("/.git") || arg.contains("/.git/")) => Some(DestructiveGit),
        "docker" | "podman" if publishes_image(texts) => Some(ExternalWrite),
        "rm" if (short('r') || short('R') || has(&["--recursive"]))
            && texts.iter().any(|arg| sweeping(arg, home)) => Some(DestructiveGit),
        _ => None,
    };
    // A subcommand only the shell knows could be any of the above.
    let unknown = decided_by_arguments && first_word.is_some_and(|word| word.dynamic || word.glob);
    if unknown && !matches!(found, Some(Administrator)) {
        Some(Unclassified)
    } else {
        found
    }
}

/// Whether a container command sends its result to a registry without a `push` word.
fn publishes_image(args: &[&str]) -> bool {
    args.iter().any(|arg| *arg == "--push" || arg.starts_with("--push=")
        // Build outputs and caches: `--output type=registry`, `type=image,push=true`.
        || ["type=registry", "push=true", "push=1"].iter().any(|value| arg.contains(value)))
        || args.contains(&"compose") && args.contains(&"publish")
        || args.contains(&"imagetools") && args.contains(&"create")
}

/// Whether deleting `target` recursively could take a whole tree: the root, home,
/// the working directory, a parent, or everything inside one of them.
fn sweeping(target: &str, home: Option<&str>) -> bool {
    // An empty value in front of `/*` or a name leaves a path elsewhere.
    if target.starts_with(UNKNOWN) && !target.trim_start_matches(UNKNOWN).is_empty() {
        return true;
    }
    // Any other unknown part may be empty too.
    let target = target.replace(UNKNOWN, "");
    if target.is_empty() {
        return false;
    }
    let mut parts = target.split('/').filter(|part| !matches!(*part, "" | ".")).peekable();
    parts.next_if_eq(&"~");
    parts.all(|part| matches!(part, ".." | "*" | "**" | ".*"))
        || home.is_some_and(|home| target.trim_end_matches(['/', '*', '.']) == home)
}

fn github(args: &[Word]) -> Option<Reason> {
    let operands: Vec<_> = args.iter().filter(|arg| !arg.text.starts_with('-')).collect();
    let words: Vec<_> = operands.iter().map(|arg| arg.text.as_str()).collect();
    if words.starts_with(&["auth", "token"])
        || words.starts_with(&["auth", "status"]) && args.iter().any(|arg| arg.text == "--show-token")
    {
        return Some(Credentials);
    }
    let area = words.first().copied().unwrap_or_default();
    // The area and, outside `api`, the verb decide whether this writes.
    let decisive = if area == "api" { 1 } else { 2 };
    if operands.iter().take(decisive).any(|word| word.dynamic) {
        return Some(Unclassified);
    }
    let read = match area {
        "" | "help" | "version" | "search" | "status" | "completion" => true,
        "api" if args.iter().any(|arg| arg.dynamic
            && (arg.text.starts_with(UNKNOWN) || arg.text.starts_with('-'))) => return Some(Unclassified),
        "api" => !args.iter().enumerate().any(|(index, arg)| {
            let arg = arg.text.as_str();
            let method = arg.strip_prefix("--method=").or_else(|| arg.strip_prefix("-X").filter(|rest| !rest.is_empty()))
                .or_else(|| matches!(arg, "-X" | "--method")
                    .then(|| args.get(index + 1).map(|next| next.text.as_str()).unwrap_or_default()));
            method.is_some_and(|method| !method.eq_ignore_ascii_case("get"))
                || arg.starts_with("-f") || arg.starts_with("-F") || arg.starts_with("--field")
                || arg.starts_with("--raw-field") || arg.starts_with("--input")
        }),
        _ => matches!(words.get(1).copied().unwrap_or_default(),
            "view" | "list" | "status" | "diff" | "checks" | "watch" | "download" | "clone" | "checkout"),
    };
    (!read).then_some(ExternalWrite)
}
