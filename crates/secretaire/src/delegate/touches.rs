use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use fuzukue_proto::{DelegateFileObservation, DelegateWriteSetSource};
use globset::GlobBuilder;
use regex_automata::dfa::{dense, Automaton};
use regex_automata::nfa::thompson;
use regex_automata::util::syntax;
use regex_automata::Input;

type Dfa = dense::DFA<Vec<u32>>;

struct Pattern {
    relative: String,
    dfa: Dfa,
}

impl Pattern {
    fn new(root: &Path, relative: String) -> Result<Self> {
        let relative = relative.strip_prefix("./").unwrap_or(&relative);
        ensure!(
            !relative.contains(['\\', ':'])
                && !relative.chars().any(char::is_control)
                && relative
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | "..")),
            "touches entries must be workspace-relative path globs: {relative:?}"
        );
        let root = root.to_str().context("non-UTF-8 workspace path")?;
        let full = format!("{}/{relative}", globset::escape(root.trim_end_matches('/')));
        let glob = GlobBuilder::new(&full)
            .literal_separator(true)
            .backslash_escape(false)
            .build()?;
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .dfa_size_limit(Some(256 * 1024))
                    .determinize_size_limit(Some(1024 * 1024)),
            )
            .syntax(syntax::Config::new().utf8(false))
            .thompson(thompson::Config::new().utf8(false))
            .build(glob.regex())
            .with_context(|| format!("compile touches pattern {relative:?}"))?;
        Ok(Self {
            relative: relative.into(),
            dfa,
        })
    }

    fn matches(&self, path: &Path) -> bool {
        self.dfa
            .try_search_fwd(&Input::new(path.as_os_str().as_encoded_bytes()))
            .is_ok_and(|hit| hit.is_some())
    }

    // Search the product of the two automata. Equal transitions are visited
    // once, even when a wildcard accepts many different bytes.
    fn overlaps(&self, other: &Self) -> bool {
        let a = &self.dfa;
        let b = &other.dfa;
        let input = Input::new(b"");
        let first = (
            a.start_state_forward(&input)
                .expect("no look-around or quit bytes"),
            b.start_state_forward(&input)
                .expect("no look-around or quit bytes"),
        );
        let mut seen = HashSet::from([first]);
        let mut pending = vec![first];
        while let Some((left, right)) = pending.pop() {
            if a.is_match_state(a.next_eoi_state(left)) && b.is_match_state(b.next_eoi_state(right))
            {
                return true;
            }
            for byte in 1..=u8::MAX {
                let left = a.next_state(left, byte);
                let right = b.next_state(right, byte);
                if !a.is_dead_state(left) && !b.is_dead_state(right) && seen.insert((left, right)) {
                    if seen.len() > 16_384 {
                        return true;
                    }
                    pending.push((left, right));
                }
            }
        }
        false
    }
}

pub(crate) struct WriteSet {
    root: PathBuf,
    source: DelegateWriteSetSource,
    patterns: Option<Vec<Pattern>>,
}

impl WriteSet {
    pub(crate) fn new(
        root: &Path,
        declared: Option<Vec<String>>,
        prompt: &str,
        previous: impl FnOnce() -> Option<Vec<String>>,
    ) -> Result<Self> {
        use DelegateWriteSetSource::*;
        let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        let compile = |globs: Vec<String>| {
            globs
                .into_iter()
                .map(|glob| Pattern::new(&root, glob))
                .collect::<Result<Vec<_>>>()
        };
        let (source, patterns) = if let Some(globs) = declared {
            (Declared, Some(compile(globs)?))
        } else {
            let inferred = compile(infer(&root, prompt))
                .ok()
                .filter(|patterns| !patterns.is_empty());
            if let Some(patterns) = inferred {
                (Prompt, Some(patterns))
            // A previous read-only declaration gives no path information for
            // a new, ambiguous assignment.
            } else if let Some(globs) = previous().filter(|globs| !globs.is_empty()) {
                (PreviousJob, compile(globs).ok())
            } else {
                (Unknown, None)
            }
        };
        let source = if patterns.is_none() { Unknown } else { source };
        Ok(Self {
            root,
            source,
            patterns,
        })
    }

    pub(crate) fn known(&self) -> bool {
        self.patterns.is_some()
    }

    pub(crate) fn globs(&self) -> Option<Vec<String>> {
        self.patterns.as_ref().map(|patterns| {
            patterns
                .iter()
                .map(|pattern| pattern.relative.clone())
                .collect()
        })
    }

    pub(super) fn declared_globs(&self) -> Option<Vec<String>> {
        (self.source == DelegateWriteSetSource::Declared).then(|| self.globs()).flatten()
    }

    /// The active job's conflicting glob, or an unknown write set.
    pub(crate) fn overlap<'a>(&self, active: &'a Self) -> Option<Option<&'a str>> {
        let patterns = self.patterns.as_deref()?;
        if patterns.is_empty() {
            return None;
        }
        let Some(other) = &active.patterns else {
            return Some(None);
        };
        other
            .iter()
            .find(|b| patterns.iter().any(|a| a.overlaps(b)))
            .map(|pattern| Some(pattern.relative.as_str()))
    }

    pub(crate) fn outside<'a>(&self, paths: &'a [PathBuf]) -> Vec<&'a Path> {
        let Some(patterns) = &self.patterns else {
            return Vec::new();
        };
        paths
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| {
                let full = self.root.join(path);
                !patterns.iter().any(|pattern| pattern.matches(&full))
            })
            .collect()
    }

    pub(crate) fn observation(&self, paths: Option<&[PathBuf]>) -> DelegateFileObservation {
        DelegateFileObservation {
            source: self.source,
            touches: self.globs(),
            changed_files: paths.map(|paths| {
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect()
            }),
        }
    }
}

fn infer(root: &Path, prompt: &str) -> Vec<String> {
    let tokens: BTreeSet<_> = prompt
        .split(|c: char| {
            !c.is_alphanumeric()
                && !matches!(c, '/' | '\\' | '.' | '_' | '-' | '+' | '@' | '*' | '?')
        })
        .map(|token| token.trim_end_matches('.'))
        .collect();
    let mut globs = BTreeSet::new();
    let mut covered_crates = BTreeSet::new();
    for token in &tokens {
        let relative = token.strip_prefix("./").unwrap_or(token);
        let path = Path::new(relative);
        if relative.contains('\\')
            || path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            continue;
        }
        let full = root.join(path);
        let directory = full.is_dir() && !relative.is_empty();
        let file = full.is_file()
            || relative.contains('.')
                && path.parent().is_some_and(|parent| {
                    !parent.as_os_str().is_empty() && root.join(parent).is_dir()
                });
        let wildcard = relative.contains(['*', '?'])
            && relative
                .split_once(['*', '?'])
                .and_then(|(prefix, _)| prefix.rsplit_once('/'))
                .is_some_and(|(parent, _)| root.join(parent).is_dir());
        if !relative.contains('/')
            && !file
            && !directory
            && relative.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty() && ext.starts_with(|c: char| c.is_ascii_alphabetic())
            })
        {
            globs.insert(format!("**/{}", globset::escape(relative)));
        }
        if !(directory || file || wildcard) {
            continue;
        }
        let relative = relative.trim_end_matches('/');
        let glob = if wildcard {
            relative.to_owned()
        } else {
            globset::escape(relative)
        };
        globs.insert(if directory {
            format!("{glob}/**")
        } else {
            glob
        });
        if let Some(name) = relative
            .strip_prefix("crates/")
            .and_then(|path| path.split('/').next())
        {
            covered_crates.insert(name);
        }
    }
    // Cargo package names are commonly used without their directory prefix.
    if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
        for entry in crates.flatten() {
            let name = entry.file_name();
            if let Some(name) = name
                .to_str()
                .filter(|name| tokens.contains(name) && !covered_crates.contains(name))
            {
                if entry.path().join("Cargo.toml").is_file() {
                    globs.insert(format!("crates/{}/**", globset::escape(name)));
                }
            }
        }
    }
    globs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_uses_specific_paths_before_crate_and_previous_job_inference() -> Result<()> {
        let root = tempfile::tempdir()?;
        let crate_dir = root.path().join("crates/secretaire");
        std::fs::create_dir_all(crate_dir.join("src"))?;
        std::fs::write(crate_dir.join("Cargo.toml"), "")?;
        let guess =
            |prompt| WriteSet::new(root.path(), None, prompt, || Some(vec!["README.md".into()]));
        let job = guess("In secretaire, add crates/secretaire/src/admission.rs.")?;
        assert_eq!(
            job.globs(),
            Some(vec!["crates/secretaire/src/admission.rs".into()])
        );
        assert_eq!(job.source, DelegateWriteSetSource::Prompt);
        assert_eq!(
            guess("Run tests in secretaire")?.globs(),
            Some(vec!["crates/secretaire/**".into()])
        );
        assert_eq!(
            guess("Edit chat.rs")?.globs(),
            Some(vec!["**/chat.rs".into()])
        );
        assert_eq!(
            guess("Continue the previous work")?.source,
            DelegateWriteSetSource::PreviousJob
        );
        assert_eq!(
            guess("Continue the previous work")?.globs(),
            Some(vec!["README.md".into()])
        );
        assert!(
            WriteSet::new(root.path(), Some(vec!["../unrelated".into()]), "", || None).is_err()
        );
        Ok(())
    }

    #[test]
    fn paths_under_nested_workspaces_are_compared_on_the_same_filesystem() -> Result<()> {
        let outer = WriteSet::new(
            Path::new("/fixture"),
            Some(vec!["nested/**/{a,b}.rs".into()]),
            "",
            || None,
        )?;
        let inner = WriteSet::new(
            Path::new("/fixture/nested"),
            Some(vec!["src/[ab].rs".into()]),
            "",
            || None,
        )?;
        assert_eq!(outer.overlap(&inner), Some(Some("src/[ab].rs")));
        let changed = ["src/a.rs", "src/c.rs"].map(PathBuf::from);
        assert_eq!(inner.outside(&changed), vec![Path::new("src/c.rs")]);
        Ok(())
    }
}
