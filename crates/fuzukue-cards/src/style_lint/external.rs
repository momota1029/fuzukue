use super::*;
use anyhow::{anyhow, bail, ensure};
use std::fs::{self, File};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct Finding<'a> {
    pub card: &'a crate::Card,
    pub rule: String,
    pub message: String,
    pub position: Position,
}

#[derive(Deserialize)]
struct Report {
    #[serde(rename = "filePath")]
    file: PathBuf,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "ruleId")]
    rule: Option<String>,
    message: String,
    line: Option<usize>,
    column: Option<usize>,
}

struct Export {
    markdown: String,
    lines: Vec<Vec<Option<Position>>>,
}

impl Export {
    fn new(prose: &MappedText) -> Self {
        let mut export = Self {
            markdown: String::new(),
            lines: Vec::new(),
        };
        for line in prose.text.split_inclusive('\n') {
            let line = line.trim_end_matches('\n');
            let mut positions = Vec::new();
            if !line
                .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .is_empty()
            {
                for (index, c) in line.char_indices() {
                    if c == '\0' {
                        let placeholder = "`…`";
                        export.markdown.push_str(placeholder);
                        positions.extend(std::iter::repeat_n(
                            None,
                            placeholder.encode_utf16().count(),
                        ));
                    } else {
                        export.markdown.push(c);
                        positions.extend(std::iter::repeat_n(
                            Some(prose.source(line)[index]),
                            c.len_utf16(),
                        ));
                    }
                }
            }
            export.markdown.push('\n');
            export.lines.push(positions);
        }
        export
    }

    fn position(&self, line: usize, column: usize) -> Option<Position> {
        let positions = self.lines.get(line.checked_sub(1)?)?;
        let index = column.checked_sub(1)?;
        // Some rules point after the final character (for example a missing period).
        let index = if index == positions.len() {
            index.checked_sub(1)?
        } else {
            index
        };
        positions.get(index).copied().flatten()
    }
}

pub(super) fn check<'a>(
    root: &Path,
    settings: &WorkspaceStyle,
    cards: &[&'a crate::Card],
) -> Result<Vec<Finding<'a>>> {
    let mode = settings.textlint.as_deref().unwrap_or("auto");
    if mode == "off" {
        return Ok(Vec::new());
    }
    let Some(config) = [".textlintrc", ".textlintrc.json", ".textlintrc.yml"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
    else {
        if mode != "auto" {
            bail!("No .textlintrc in {}", root.display());
        }
        return Ok(Vec::new());
    };

    let dir = tempfile::tempdir()?;
    let mut exports = BTreeMap::new();
    for (index, &card) in cards.iter().enumerate() {
        let export = Export::new(&prose(&card.body, true));
        if export.markdown.trim().is_empty() {
            continue;
        }
        let filename = format!("{index}.md");
        fs::write(dir.path().join(&filename), &export.markdown)?;
        exports.insert(filename, (card, export));
    }
    if exports.is_empty() {
        return Ok(Vec::new());
    }

    let local = root.join("node_modules/.bin").join(if cfg!(windows) {
        "textlint.cmd"
    } else {
        "textlint"
    });
    let npx = vec![
        "npx".into(),
        "--offline".into(),
        "--no-install".into(),
        "--".into(),
        "textlint".into(),
    ];
    let commands = if mode == "auto" {
        let mut commands = Vec::new();
        if local.is_file() {
            commands.push(vec![local.to_string_lossy().into_owned()]);
        }
        commands.extend([vec!["textlint".into()], npx]);
        commands
    } else {
        let args = shlex::split(mode)
            .filter(|args| !args.is_empty())
            .context("Invalid textlint command")?;
        if args == ["npx", "textlint"] {
            vec![npx]
        } else {
            vec![args]
        }
    };

    let stdout = dir.path().join("stdout.json");
    let stderr = dir.path().join("stderr.txt");
    let (elapsed, status) = {
        let started = Instant::now();
        let mut child = None;
        for args in commands {
            let file = Path::new(&args[0]);
            let executable = if file.is_relative() && file.components().count() > 1 {
                root.join(file)
            } else {
                file.to_path_buf()
            };
            let mut command = Command::new(executable);
            command
                .args(&args[1..])
                .current_dir(root)
                .env("npm_config_offline", "true")
                .env("npm_config_yes", "false")
                .args(["--format", "json", "--config"])
                .arg(&config)
                .args(exports.keys().map(|filename| dir.path().join(filename)))
                .stdin(Stdio::null())
                .stdout(File::create(&stdout)?)
                .stderr(File::create(&stderr)?);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            match command.spawn() {
                Ok(process) => {
                    child = Some(process);
                    break;
                }
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => return Err(err).context("Could not start textlint"),
            }
        }
        let mut child = child.context("No textlint executable on PATH")?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= Duration::from_secs(30) {
                #[cfg(unix)]
                {
                    // npx launches another process. Stop its process group too.
                    let _ = Command::new("kill")
                        .args(["-KILL", "--", &format!("-{}", child.id())])
                        .status();
                }
                let _ = child.kill();
                let _ = child.wait();
                bail!("textlint timed out");
            }
            thread::sleep(Duration::from_millis(15));
        };
        (started.elapsed(), status)
    };

    let reports = serde_json::from_reader::<_, Vec<Report>>(File::open(stdout)?);
    let reports = match reports {
        Ok(reports) if matches!(status.code(), Some(0 | 1)) => reports,
        _ => {
            let detail = fs::read_to_string(stderr).unwrap_or_default();
            let detail = detail.trim().chars().take(500).collect::<String>();
            return Err(anyhow!(
                "textlint returned {status} after {elapsed:.1?}: {detail}"
            ));
        }
    };
    let mut findings = Vec::new();
    for report in reports {
        let filename = report
            .file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some((card, export)) = exports.get(filename) else {
            continue;
        };
        for message in report.messages {
            let Some(rule) = message.rule else {
                bail!("{}", message.message);
            };
            let (Some(line), Some(column)) = (message.line, message.column) else {
                continue;
            };
            ensure!(!rule.is_empty(), "textlint returned an empty rule name");
            if let Some(position) = export.position(line, column) {
                findings.push(Finding {
                    card,
                    rule,
                    message: message.message,
                    position,
                });
            }
        }
    }
    Ok(findings)
}
