use std::path::Path;

use fancy_regex::Regex;

pub struct Language {
    pub key: &'static str,
    pub name: &'static str,
    pub listings: &'static str,
    pub extensions: &'static [&'static str],
    pub comments: &'static [&'static str],
    pub index_base: u8,
    pub order: &'static str,
    pub api_prefix: &'static str,
    pub notes: &'static [&'static str],
}

const LANGUAGES: &[Language] = &[
    Language { key: "python", name: "Python", listings: "Python", extensions: &["py"], comments: &["#"],
        index_base: 0, order: "row", api_prefix: "",
        notes: &["Indices start at 0; slices a:b exclude b.", "numpy arrays are row-major; a 1-D array has no row/column distinction."] },
    Language { key: "matlab", name: "MATLAB", listings: "Matlab", extensions: &["m"], comments: &["%"],
        index_base: 1, order: "column", api_prefix: "m:",
        notes: &["Indices start at 1; a:b includes b.", "Matrices are column-major; ' is the conjugate transpose."] },
    Language { key: "octave", name: "Octave", listings: "Octave", extensions: &[], comments: &["%", "#"],
        index_base: 1, order: "column", api_prefix: "oct:",
        notes: &["Mostly MATLAB-compatible, but not every MATLAB function exists or behaves identically.",
            "Also .m files: use the octave override when the file is Octave."] },
    Language { key: "julia", name: "Julia", listings: "Julia", extensions: &["jl"], comments: &["#"],
        index_base: 1, order: "column", api_prefix: "jl:",
        notes: &["Indices start at 1; a:b includes b.", "f.(x) broadcasts f over x; a trailing ! means the function mutates its argument.",
            "Arrays are column-major."] },
    Language { key: "r", name: "R", listings: "R", extensions: &["r"], comments: &["#"],
        index_base: 1, order: "column", api_prefix: "r:",
        notes: &["Indices start at 1; x[a:b] includes b and a negative index drops elements.",
            "Matrices are column-major; data frames are lists of columns, not matrices."] },
    Language { key: "cpp", name: "C++", listings: "C++", extensions: &["cpp", "cc", "cxx", "hpp", "hh", "h"], comments: &["//"],
        index_base: 0, order: "row", api_prefix: "cpp:",
        notes: &["Indices start at 0; there is no bounds check with operator[].",
            "Check ownership: a & is a reference (no copy), a value argument is copied.",
            "Libraries differ: Eigen is column-major by default, plain arrays are row-major."] },
];

pub const JULIA: &str = r#"\lstdefinelanguage{Julia}{morekeywords={abstract,begin,break,catch,const,continue,do,else,elseif,end,export,finally,for,function,global,if,import,in,let,local,macro,module,mutable,quote,return,struct,try,using,where,while,true,false,nothing},sensitive=true,morecomment=[l]{\#},morecomment=[n]{\#=}{=\#},morestring=[b]\"}"#;

pub fn of(path: &str, override_key: &str) -> Option<&'static Language> {
    if !override_key.is_empty() {
        return LANGUAGES
            .iter()
            .find(|language| language.key == override_key);
    }
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|language| language.extensions.contains(&extension.as_str()))
}

impl Language {
    pub fn calls(&self, text: &str, name: &str) -> bool {
        let text = text
            .lines()
            .map(|line| {
                self.comments.iter().fold(line, |line, token| {
                    line.split_once(token).map_or(line, |(code, _)| code)
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (before, after) = match self.key {
            "matlab" | "octave" => (r"(?<![\w.])", r"\b"),
            "julia" => (r"(?<![\w.])", r"(?:!|\.)?\s*\("),
            "cpp" => (r"(?<!\w)", r"\s*[\(<{]"),
            _ => (r"(?<![\w.])", r"\s*\("),
        };
        Regex::new(&format!("{before}{}{after}", regex::escape(name)))
            .is_ok_and(|regex| regex.is_match(&text).unwrap_or(false))
    }
}
