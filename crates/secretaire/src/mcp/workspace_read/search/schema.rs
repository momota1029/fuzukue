use serde_json::{json, Value};

use super::filters::{MAX_FILTERS, MAX_GLOB_BYTES, TYPES};
use super::matcher::{MAX_PATTERNS, MAX_PATTERN_BYTES};
use super::{CONTEXT, COUNT, DEFAULT_CONTEXT, DEFAULT_RESULTS, MAX_PATHS, RESULTS};

pub(in crate::mcp::workspace_read) fn definition(
    scoped_path: &str,
) -> (&'static str, Value, &'static [&'static str]) {
    let description = harness_text!(
        "Use this instead of rg/grep/find to search scoped UTF-8 files or list matching paths. Directories are searched recursively. Example: rg -t rust -S -w -g '!tests/**' -e TODO src → {\"path\":\"src\",\"pattern\":\"TODO\",\"types\":[\"rust\"],\"word\":true,\"smart_case\":true,\"globs\":[\"!tests/**\"]}. rg --files -g '*.rs' → {\"output_mode\":\"files\",\"glob\":\"*.rs\"}. rg -U, --column, --sort modified, --stats, --hidden and --no-ignore map to multiline=true, column=true, sort=modified, stats=true, include_hidden=true and include_ignored=true. For sizes and timestamps use fs_list.\n\n| grep flags | fs_search arguments |\n| --- | --- |\n| -r / -R | Recursion is automatic. Symlinks encountered in traversal are skipped |\n| -n / -i | line_number=true (default) / ignore_case=true |\n| -l / -L / -c | output_mode=files_with_matches / files_without_matches / count |\n| -E / -F | Regex by default / fixed_string=true |\n| -v / -w / -x | invert=true / word=true / whole_line=true |\n| -A / -B / -C | after_context / before_context / context |\n| -m | max_count per file |\n| --include / --exclude / --exclude-dir | globs=['*.rs','!sample.rs','!fixtures/'] |\n| -o / -h / -H | only_matching=true / with_path=false / with_path=true |\n| -q | output_mode=count and max_results=1. A count row means a match |\n| -e / -- before a pattern | Pass text such as '-draft' as pattern, or use patterns for multiple -e flags |",
        "rg、grep、find の代わりに使う。読み取り範囲内の UTF-8 ファイルを再帰検索し、パスだけの一覧も返せる。例: rg -t rust -S -w -g '!tests/**' -e TODO src → {\"path\":\"src\",\"pattern\":\"TODO\",\"types\":[\"rust\"],\"word\":true,\"smart_case\":true,\"globs\":[\"!tests/**\"]}。rg --files -g '*.rs' → {\"output_mode\":\"files\",\"glob\":\"*.rs\"}。rg -U、--column、--sort modified、--stats、--hidden、--no-ignore はそれぞれ multiline=true、column=true、sort=modified、stats=true、include_hidden=true、include_ignored=true。サイズと更新時刻の一覧には fs_list を使う。\n\n| grep のフラグ | fs_search の引数 |\n| --- | --- |\n| -r / -R | ディレクトリは再帰検索。走査中に見つけたシンボリックリンクはたどらない |\n| -n / -i | line_number=true（既定） / ignore_case=true |\n| -l / -L / -c | output_mode=files_with_matches / files_without_matches / count |\n| -E / -F | 正規表現が既定 / fixed_string=true |\n| -v / -w / -x | invert=true / word=true / whole_line=true |\n| -A / -B / -C | after_context / before_context / context |\n| -m | ファイルごとの max_count |\n| --include / --exclude / --exclude-dir | globs=['*.rs','!sample.rs','!fixtures/'] |\n| -o / -h / -H | only_matching=true / with_path=false / with_path=true |\n| -q | output_mode=count と max_results=1。一致があれば集計行が返る |\n| -e / パターン直前の -- | '-draft' のような文字列を pattern に渡す。複数の -e は patterns |"
    );
    let array = |max, items| json!({"type": "array", "maxItems": max, "items": items});
    let pattern = json!({
        "type": "string", "maxLength": MAX_PATTERN_BYTES,
        "description": harness_text!(
            "Regex or literal text, including a leading '-'. Required when patterns is absent except in files mode. Set multiline=true to span lines. The regex engine supports no grep -P lookaround or backreferences.",
            "先頭が '-' の文字列もそのまま渡せる。files モード以外は pattern または patterns が必要。複数行にまたがる検索には multiline=true を指定する。grep -P の先読み、後読み、後方参照の構文は利用できない。"
        )
    });
    let glob = json!({"type": "string", "maxLength": MAX_GLOB_BYTES});
    let types =
        json!({"type": "string", "enum": TYPES.iter().map(|(name, _)| *name).collect::<Vec<_>>()});
    (
        description,
        json!({
            "path": {"type": "string", "description": scoped_path},
            "paths": {
                "type": "array", "maxItems": MAX_PATHS,
                "items": {"type": "string", "description": scoped_path},
                "description": harness_text!(
                    "Additional search roots, processed in order after path. Defaults to the working directory when no roots are supplied. With paths, results are grouped by root and include root-qualified paths. max_results applies to each root; byte and time limits apply to the whole search.",
                    "追加の検索起点。path の後に指定順で検索する。起点を指定しない場合は作業ディレクトリを検索する。paths を指定すると、結果を起点順にまとめ、パスに起点を含める。max_results は起点ごと、バイト数と時間の上限は検索全体に適用する。"
                )
            },
            "pattern": pattern,
            "patterns": array(MAX_PATTERNS, pattern),
            "glob": glob,
            "globs": {
                "type": "array", "maxItems": MAX_FILTERS, "items": glob,
                "description": harness_text!(
                    "Ordered globs, after glob, relative to path. Prefix ! to exclude, including !dir/ for a directory. The last matching glob wins.",
                    "glob に続く順序付きの glob。path からの相対パスに照合する。! を付けると除外し、!dir/ でディレクトリを除外する。最後に合致した glob を適用する。"
                )
            },
            "types": array(MAX_FILTERS, types.clone()),
            "exclude_types": array(MAX_FILTERS, types),
            "fixed_string": {"type": "boolean", "default": false},
            "ignore_case": {"type": "boolean", "default": false},
            "smart_case": {"type": "boolean", "default": false},
            "word": {"type": "boolean", "default": false},
            "whole_line": {"type": "boolean", "default": false},
            "invert": {"type": "boolean", "default": false},
            "only_matching": {"type": "boolean", "default": false},
            "multiline": {"type": "boolean", "default": false},
            "include_hidden": {"type": "boolean", "default": false},
            "include_ignored": {"type": "boolean", "default": false,
                "description": harness_text!(
                    "Skip scoped .gitignore and .ignore rules. Use an explicit path to search generated directories. Git metadata and protected paths stay excluded.",
                    "読み取り範囲内の .gitignore と .ignore の規則を適用せずに検索する。生成先は path に明示指定する。Git の内部データと保護対象は検索範囲外。"
                )
            },
            "line_number": {"type": "boolean", "default": true},
            "column": {"type": "boolean", "default": false,
                "description": harness_text!("Return the first match's one-based byte column.", "最初の一致位置を 1 始まりのバイト列番号で返す。")
            },
            "with_path": {"type": "boolean", "default": true},
            "stats": {"type": "boolean", "default": false},
            "context": CONTEXT.property(DEFAULT_CONTEXT),
            "before_context": CONTEXT.schema(),
            "after_context": CONTEXT.schema(),
            "max_count": COUNT.schema(),
            "max_results": RESULTS.property(DEFAULT_RESULTS),
            "sort": {"type": "string", "enum": ["path", "modified"], "default": "path",
                "description": harness_text!("Paths in ascending order, or oldest modification first with path as a tiebreaker.",
                    "パスの昇順、または更新時刻の古い順。同じ時刻ならパスの昇順。")
            },
            "output_mode": {
                "type": "string", "enum": ["content", "files_with_matches", "files_without_matches", "count", "files"], "default": "content",
                "description": harness_text!(
                    "Content returns matching lines and context, or substrings with only_matching. Count returns matching-line counts per matching file. The files modes return paths. Use files without a pattern. max_results caps returned rows per root.",
                    "content は一致行と前後の行を返し、only_matching では一致部分を返す。count は一致したファイルごとの一致行数を返す。files 系モードはパスを返す。files ではパターンを省略する。max_results は起点ごとに返す行数の上限。"
                )
            }
        }),
        &[],
    )
}
