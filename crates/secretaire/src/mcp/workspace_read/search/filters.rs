use anyhow::{ensure, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use std::path::Path;

use super::Args;

pub(super) const MAX_FILTERS: usize = 16;
pub(super) const MAX_GLOB_BYTES: usize = 1024;

pub(super) const TYPES: &[(&str, &[&str])] = &[
    ("c", &["*.[chH]", "*.[chH].in", "*.cats"]),
    (
        "cpp",
        &[
            "*.[ChH]",
            "*.[ChH].in",
            "*.[ch]pp",
            "*.[ch]pp.in",
            "*.[ch]xx",
            "*.[ch]xx.in",
            "*.cc",
            "*.cc.in",
            "*.hh",
            "*.hh.in",
            "*.inl",
        ],
    ),
    ("css", &["*.css", "*.scss"]),
    ("go", &["*.go"]),
    ("html", &["*.html", "*.htm", "*.ejs"]),
    ("java", &["*.java", "*.jsp", "*.jspx", "*.properties"]),
    ("js", &["*.cjs", "*.js", "*.jsx", "*.mjs", "*.vue"]),
    ("json", &["*.json", "*.sarif", "composer.lock"]),
    ("kt", &["*.kt", "*.kts"]),
    ("kotlin", &["*.kt", "*.kts"]),
    ("lean", &["*.lean"]),
    (
        "md",
        &[
            "*.markdown",
            "*.md",
            "*.mdown",
            "*.mdwn",
            "*.mdx",
            "*.mkd",
            "*.mkdn",
        ],
    ),
    ("py", &["*.py", "*.pyi"]),
    ("ruby", &["*.rb", "*.gemspec", "Gemfile", "Rakefile"]),
    ("rust", &["*.rs"]),
    (
        "sh",
        &[
            "*.bash",
            "*.bashrc",
            "*.csh",
            "*.cshrc",
            "*.env",
            "*.ksh",
            "*.kshrc",
            "*.sh",
            "*.tcsh",
            "*.zsh",
            ".bash_login",
            ".bash_logout",
            ".bash_profile",
            ".bashrc",
            ".cshrc",
            ".env",
            ".kshrc",
            ".login",
            ".logout",
            ".profile",
            ".tcshrc",
            ".zlogin",
            ".zlogout",
            ".zprofile",
            ".zshenv",
            ".zshrc",
            "bash_login",
            "bash_logout",
            "bash_profile",
            "bashrc",
            "profile",
            "zlogin",
            "zlogout",
            "zprofile",
            "zshenv",
            "zshrc",
        ],
    ),
    ("toml", &["*.toml", "Cargo.lock"]),
    ("ts", &["*.cts", "*.mts", "*.ts", "*.tsx"]),
    ("txt", &["*.txt"]),
    ("xml", &["*.xml", "*.xsd", "*.xsl", "*.xslt"]),
    ("yaml", &["*.yaml", "*.yml"]),
];

pub(super) struct Filters {
    globs: Vec<GlobRule>,
    include_types: Vec<GlobMatcher>,
    exclude_types: Vec<GlobMatcher>,
    has_positive: bool,
}

struct GlobRule {
    matcher: GlobMatcher,
    basename: bool,
    negative: bool,
    directory_only: bool,
}

impl GlobRule {
    fn new(raw: &str, parameter: &str) -> Result<Self> {
        ensure!(
            raw.len() <= MAX_GLOB_BYTES && !raw.contains(['\r', '\n', '\0']),
            "{parameter} must be a single-line glob of at most {MAX_GLOB_BYTES} bytes"
        );
        let (negative, pattern) = raw
            .strip_prefix('!')
            .map_or((false, raw), |rest| (true, rest));
        let (directory_only, pattern) = pattern
            .strip_suffix('/')
            .map_or((false, pattern), |rest| (true, rest));
        let basename = !pattern.contains('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        ensure!(!pattern.is_empty(), "{parameter} must contain a glob");
        ensure!(
            !directory_only || negative,
            "{parameter} must use path/** to include the contents of a directory"
        );
        let matcher = compile(pattern).with_context(|| format!("Invalid {parameter}"))?;
        Ok(Self {
            matcher,
            basename,
            negative,
            directory_only,
        })
    }

    fn matches_path(&self, path: &Path) -> bool {
        self.matcher.is_match(path)
            || self.basename
                && path
                    .file_name()
                    .is_some_and(|name| self.matcher.is_match(name))
    }

    fn matches_file(&self, path: &Path) -> bool {
        (!self.directory_only && self.matches_path(path))
            || self.negative
                && path
                    .ancestors()
                    .skip(1)
                    .take_while(|parent| !parent.as_os_str().is_empty())
                    .any(|parent| self.matches_path(parent))
    }
}

impl Filters {
    pub(super) fn new(args: &Args) -> Result<Self> {
        ensure!(
            args.globs.len() <= MAX_FILTERS,
            "globs accepts at most {MAX_FILTERS} entries"
        );
        let mut globs = Vec::new();
        if let Some(glob) = &args.glob {
            globs.push(GlobRule::new(glob, "glob")?);
        }
        for (index, glob) in args.globs.iter().enumerate() {
            globs.push(GlobRule::new(glob, &format!("globs[{index}]"))?);
        }
        let has_positive = globs.iter().any(|rule| !rule.negative);
        Ok(Self {
            globs,
            include_types: type_globs(&args.types, "types")?,
            exclude_types: type_globs(&args.exclude_types, "exclude_types")?,
            has_positive,
        })
    }

    pub(super) fn matches(&self, path: &Path) -> bool {
        let glob_matches = self
            .globs
            .iter()
            .rev()
            .find(|rule| rule.matches_file(path))
            .map_or(!self.has_positive, |rule| !rule.negative);
        glob_matches
            && path.file_name().is_some_and(|name| {
                (self.include_types.is_empty()
                    || self.include_types.iter().any(|glob| glob.is_match(name)))
                    && !self.exclude_types.iter().any(|glob| glob.is_match(name))
            })
    }
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

fn type_globs(names: &[String], parameter: &str) -> Result<Vec<GlobMatcher>> {
    ensure!(
        names.len() <= MAX_FILTERS,
        "{parameter} accepts at most {MAX_FILTERS} entries"
    );
    let mut result = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let (_, patterns) = TYPES.iter().find(|(known, _)| known == name).with_context(|| {
            let names = TYPES.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ");
            format!("Unknown {parameter}[{index}]. Supported types: {names}. Use glob or globs for other file names.")
        })?;
        for pattern in *patterns {
            result.push(compile(pattern)?);
        }
    }
    Ok(result)
}
