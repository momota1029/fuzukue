use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rustix::fs::{self as unix, Mode, OFlags};
use rustix::io::Errno;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::paths::{self, ReadScope};

const FILE_BYTES: usize = 64 * 1024;
const TOTAL_BYTES: usize = 256 * 1024;
const MAX_RULES: usize = 1024;
const MAX_RULE_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct Ignores {
    layers: Vec<Layer>,
    bytes: usize,
    rules: usize,
    matches: Vec<usize>,
}

struct Layer {
    directory: PathBuf,
    is_ignore: bool,
    globs: GlobSet,
    rules: Vec<Rule>,
}

struct Rule {
    include: bool,
    directory_only: bool,
}

impl Ignores {
    pub(super) fn checkpoint(&self) -> usize {
        self.layers.len()
    }

    pub(super) fn restore(&mut self, checkpoint: usize) {
        self.layers.truncate(checkpoint);
    }

    // Read rules through the same descriptors and scope checks used for search files.
    // Return false so an unreadable or over-budget rule file cannot silently widen a scan.
    pub(super) fn load(
        &mut self,
        scope: &ReadScope,
        directory: &Path,
        handle: &File,
        deadline: Instant,
    ) -> bool {
        [".gitignore", ".ignore"].into_iter().all(|name| {
            let path = directory.join(name);
            if scope.check(&path).is_err() {
                return true;
            }
            let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            let file = match unix::openat(handle, name, flags, Mode::empty()) {
                Ok(fd) => File::from(fd),
                Err(Errno::NOENT | Errno::LOOP) => return true,
                Err(_) => return false,
            };
            let Ok(meta) = paths::regular(&file) else {
                return true;
            };
            let cap = FILE_BYTES.min(TOTAL_BYTES - self.bytes);
            if meta.len() > cap as u64 || scope.audit_read(&path).is_err() {
                return false;
            }
            let mut bytes = Vec::new();
            if file.take((cap + 1) as u64).read_to_end(&mut bytes).is_err() || bytes.len() > cap {
                return false;
            }
            self.bytes += bytes.len();
            let Ok(text) = paths::text(&bytes) else {
                return false;
            };
            let mut globs = GlobSetBuilder::new();
            let mut rules = Vec::new();
            for line in text.strip_prefix('\u{feff}').unwrap_or(text).lines() {
                if Instant::now() >= deadline {
                    return false;
                }
                let Some((pattern, rule)) = parse_rule(line) else {
                    continue;
                };
                if pattern.len() > MAX_RULE_BYTES || self.rules == MAX_RULES {
                    return false;
                }
                if let Ok(glob) = GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .allow_unclosed_class(true)
                    .build()
                {
                    globs.add(glob);
                    rules.push(rule);
                    self.rules += 1;
                }
            }
            if !rules.is_empty() {
                let Ok(globs) = globs.build() else {
                    return false;
                };
                self.layers.push(Layer {
                    directory: directory.into(),
                    is_ignore: name == ".ignore",
                    globs,
                    rules,
                });
            }
            true
        })
    }

    pub(super) fn excludes(&mut self, path: &Path, is_dir: bool) -> bool {
        // .ignore overrides .gitignore, and the nearest file and last rule win within each kind.
        for is_ignore in [true, false] {
            for layer in self
                .layers
                .iter()
                .rev()
                .filter(|layer| layer.is_ignore == is_ignore)
            {
                let relative = path
                    .strip_prefix(&layer.directory)
                    .expect("rules are from ancestors");
                layer.globs.matches_into(relative, &mut self.matches);
                for &index in self.matches.iter().rev() {
                    let rule = &layer.rules[index];
                    if is_dir || !rule.directory_only {
                        return !rule.include;
                    }
                }
            }
        }
        false
    }
}

fn parse_rule(mut line: &str) -> Option<(String, Rule)> {
    // Unescaped trailing spaces are ignored. Escaped leading '#' and '!' are literals.
    while let Some(prefix) = line.strip_suffix(' ') {
        if prefix.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1 {
            break;
        }
        line = prefix;
    }
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (include, glob) = line
        .strip_prefix('!')
        .map_or((false, line), |glob| (true, glob));
    let (directory_only, glob) = glob
        .strip_suffix('/')
        .map_or((false, glob), |glob| (true, glob));
    let (anchored, glob) = glob
        .strip_prefix('/')
        .map_or((false, glob), |glob| (true, glob));
    if glob.is_empty() {
        return None;
    }
    let mut pattern = String::new();
    if !anchored && !glob.contains('/') && !glob.starts_with("**/") {
        pattern.push_str("**/");
    }
    // Git ignore patterns treat braces as literal characters.
    let mut escaped = false;
    for ch in glob.chars() {
        if !escaped && matches!(ch, '{' | '}') {
            pattern.push('\\');
        }
        pattern.push(ch);
        escaped = ch == '\\' && !escaped;
    }
    Some((
        pattern,
        Rule {
            include,
            directory_only,
        },
    ))
}
