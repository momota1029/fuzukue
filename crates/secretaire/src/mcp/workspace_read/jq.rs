use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(test)]
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Instant, timeout_at};

use super::range::optional_integer;
use super::{DEADLINE, MAX_BYTES, READ_BYTES, READ_SCAN_BYTES, ReadScope, open_scoped_text, paths};

const MAX_FILTER_BYTES: usize = 16 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    path: PathBuf,
    filter: String,
    #[serde(default)]
    raw: bool,
    #[serde(default, deserialize_with = "optional_integer")]
    max_bytes: Option<i128>,
}

pub(super) fn definition(scoped_path: &str) -> (&'static str, Value, &'static [&'static str]) {
    (
        harness_text!(
            r#"Use instead of shell jq to select fields, filter records, or aggregate JSON or JSON Lines in the fs_read scope. For example, pass {"path":"events.jsonl","filter":"select(.type == \"error\") | .message","raw":true}. Filters run on each input value. Use [., inputs] to collect a stream. Uses embedded jaq. Numeric and regex behavior can differ from jq, and fromjson accepts a stream. CLI flags, module imports, and server environment access are unavailable. Takes up to 16 MiB of input and returns up to 64 KiB of output within 5 seconds."#,
            r#"シェルの jq の代わりに使う。fs_read と同じ読み取り範囲の JSON や JSON Lines からフィールドの抽出、絞り込み、集計ができる。たとえば {"path":"events.jsonl","filter":"select(.type == \"error\") | .message","raw":true} を渡す。フィルターは入力値ごとに適用する。[., inputs] で入力全体を配列にできる。組み込みの jaq を使う。jq とは数値演算や正規表現の挙動が異なる場合があり、fromjson は複数の値を読み込める。CLI オプション、モジュール読み込み、サーバーの環境変数へのアクセスは利用できない。入力は 16 MiB、出力は 64 KiB、実行時間は 5 秒が上限。"#
        ),
        json!({
            "path": {"type": "string", "description": scoped_path},
            "filter": {"type": "string", "maxLength": MAX_FILTER_BYTES},
            "raw": {"type": "boolean", "default": false, "description": harness_text!(
                "Like jq -r. Write strings without JSON quotes.",
                "jq -r と同様、文字列の JSON 引用符を省く。"
            )},
            "max_bytes": READ_BYTES.property(MAX_BYTES),
        }),
        &["path", "filter"],
    )
}

fn input(scope: &ReadScope, path: &Path) -> Result<Vec<u8>> {
    let file = open_scoped_text(scope, path)?;
    let mut bytes = Vec::new();
    file.take((READ_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= READ_SCAN_BYTES,
        "jq input exceeds the 16 MiB limit"
    );
    paths::text(&bytes)?;
    Ok(bytes)
}

#[derive(Debug)]
struct OutputLimit;

impl std::fmt::Display for OutputLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("jq output limit reached")
    }
}

impl std::error::Error for OutputLimit {}

pub(super) async fn query(scope: ReadScope, args: Args) -> Result<Value> {
    ensure!(
        !args.filter.trim().is_empty(),
        "A jq filter is required. Use . for the entire input."
    );
    ensure!(
        args.filter.len() <= MAX_FILTER_BYTES,
        "jq filter exceeds the 16 KiB limit"
    );
    ensure!(!args.filter.contains('\0'), "jq filter contains a NUL byte");

    let deadline = Instant::now() + DEADLINE;
    let path = args.path;
    let input = timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || input(&scope, &path)),
    )
    .await
    .context("jq reached the 5-second time limit while opening the input")???;
    let mut command = Command::new(std::env::current_exe()?);
    command.current_dir("/").env_clear();
    #[cfg(not(test))]
    command.arg(CHILD_COMMAND);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "mcp::workspace_read::jq::child_test",
            "--nocapture",
        ])
        .env("FUZUKUE_JQ_CHILD_TEST", "1");
    let header = serde_json::to_vec(&(args.filter, args.raw))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Could not start embedded jq")?;
    let mut stdin = child.stdin.take().context("jq stdin is unavailable")?;
    let stdout = child.stdout.take().context("jq stdout is unavailable")?;
    let mut stderr = child.stderr.take().context("jq stderr is unavailable")?;
    let max_bytes = READ_BYTES.resolve(args.max_bytes, MAX_BYTES);
    let mut output = Vec::new();
    let mut diagnostic = Vec::new();

    let run = async {
        let feed = async move {
            let result = async {
                stdin.write_all(&header).await?;
                stdin.write_all(b"\n").await?;
                stdin.write_all(&input).await
            }
            .await;
            match result {
                // jq can exit before consuming all input, for example on a compile error.
                Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
                result => Ok(result?),
            }
        };
        let collect_output = async {
            #[cfg(test)]
            let stdout = {
                let mut stdout = tokio::io::BufReader::new(stdout);
                let mut harness = Vec::new();
                stdout.read_until(0, &mut harness).await?;
                stdout
            };
            stdout
                .take((max_bytes + 1) as u64)
                .read_to_end(&mut output)
                .await?;
            ensure!(output.len() <= max_bytes, OutputLimit);
            Ok(())
        };
        let collect_diagnostic = async {
            (&mut stderr)
                .take((MAX_DIAGNOSTIC_BYTES + 1) as u64)
                .read_to_end(&mut diagnostic)
                .await?;
            // Drain extra stderr so debug output cannot block the child.
            tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
            Ok(())
        };
        let wait = async { Ok(child.wait().await?) };
        let ((), (), (), status) =
            tokio::try_join!(feed, collect_output, collect_diagnostic, wait)?;
        Ok::<_, anyhow::Error>(status)
    };
    let note = match timeout_at(deadline, run).await {
        Err(_) => Some("Output may be incomplete. jq reached the 5-second time limit.".to_owned()),
        Ok(Err(error)) if error.is::<OutputLimit>() => Some(format!(
            "Output truncated at the {max_bytes}-byte limit. Narrow the jq filter."
        )),
        Ok(result) => {
            let status = result.context("Could not read the jq result")?;
            let diagnostic_was_capped = diagnostic.len() > MAX_DIAGNOSTIC_BYTES;
            diagnostic.truncate(MAX_DIAGNOSTIC_BYTES);
            let excerpt = utf8_prefix(&diagnostic);
            let suffix = if diagnostic_was_capped {
                " … [diagnostic truncated]"
            } else {
                ""
            };
            ensure!(
                status.success(),
                "jq failed ({status}). Check the filter and that the file contains JSON or JSON Lines.\n{}{suffix}",
                excerpt.trim_end()
            );
            None
        }
    };
    if note.is_some() {
        child
            .kill()
            .await
            .context("Could not stop jq after reaching its limit")?;
    }
    output.truncate(max_bytes);
    let output = match std::str::from_utf8(&output) {
        Err(error) if note.is_some() && error.error_len().is_none() => utf8_prefix(&output).into(),
        _ => String::from_utf8_lossy(&output),
    };
    let mut result = json!({"output": output, "truncated": note.is_some()});
    if let Some(note) = note {
        result["note"] = json!(note);
    }
    Ok(result)
}

fn utf8_prefix(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix")
        }
    }
}

#[cfg(not(test))]
pub(crate) const CHILD_COMMAND: &str = "--embedded-jq-worker";

pub(crate) fn child() -> Result<()> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    // Bound memory before reading any input. Disable core dumps of scoped file contents.
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .context("Could not disable jq process dumps")?;
    let limit = |resource, max| {
        let cap = Some(getrlimit(resource).current.unwrap_or(u64::MAX).min(max));
        setrlimit(
            resource,
            Rlimit {
                current: cap,
                maximum: cap,
            },
        )
    };
    limit(Resource::Core, 0).context("Could not disable jq core dumps")?;
    limit(Resource::As, 256 * 1024 * 1024).context("Could not set the jq memory limit")?;

    let mut input = std::io::stdin().lock();
    let mut header = String::new();
    (&mut input)
        .take((MAX_FILTER_BYTES * 6 + 32) as u64)
        .read_line(&mut header)?;
    let (filter, raw): (String, bool) = serde_json::from_str(&header)?;
    let mut bytes = Vec::new();
    input
        .take((READ_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= READ_SCAN_BYTES,
        "jq input exceeds the 16 MiB limit"
    );
    evaluate(&filter, raw, &bytes, &mut std::io::stdout().lock())
}

#[cfg(test)]
#[test]
fn child_test() {
    if std::env::var_os("FUZUKUE_JQ_CHILD_TEST").is_some() {
        std::io::stdout().write_all(&[0]).unwrap();
        std::io::stdout().flush().unwrap();
        let result = child();
        if let Err(error) = &result {
            eprintln!("{error:#}");
        }
        std::process::exit(i32::from(result.is_err()));
    }
}

struct FilterData;

impl jaq_core::DataT for FilterData {
    type V<'a> = jaq_json::Val;
    type Data<'a> = InputData<'a>;
}

#[derive(Clone, Copy)]
struct InputData<'a> {
    lut: &'a jaq_core::Lut<FilterData>,
    input: jaq_std::input::Inputs<'a, jaq_json::Val>,
}

impl<'a> jaq_core::data::HasLut<'a, FilterData> for InputData<'a> {
    fn lut(&self) -> &'a jaq_core::Lut<FilterData> {
        self.lut
    }
}

impl<'a> jaq_std::input::HasInputs<'a, jaq_json::Val> for InputData<'a> {
    fn inputs(&self) -> jaq_std::input::Inputs<'a, jaq_json::Val> {
        self.input
    }
}

fn evaluate(filter: &str, raw: bool, bytes: &[u8], output: &mut dyn Write) -> Result<()> {
    use jaq_core::{
        Ctx, Vars,
        load::{Arena, File, Loader},
    };
    use jaq_json::{Val, read, write};
    use jaq_std::input::RcIter;

    // The default loader has no filesystem access. Do not add the CLI's I/O filters.
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs::<FilterData>()
        .chain(jaq_std::funs().filter(|(name, _, _)| *name != "env"))
        .chain(jaq_json::funs())
        .chain(
            jaq_std::input::funs()
                .into_vec()
                .into_iter()
                .map(jaq_core::native::run),
        );
    let filter = format!("(\n{filter}\n)");
    let arena = Arena::default();
    let modules = Loader::new(defs)
        .load(
            &arena,
            File {
                code: &filter,
                path: (),
            },
        )
        .map_err(|errors| {
            anyhow::anyhow!("jq syntax error (module imports are disabled): {errors:?}")
        })?;
    let filter = jaq_core::Compiler::default()
        .with_global_vars(["$ENV"])
        .with_funs(funs)
        .compile(modules)
        .map_err(|errors| anyhow::anyhow!("jq compile error: {errors:?}"))?;
    let input =
        RcIter::new(read::parse_many(bytes).map(|value| value.map_err(|error| error.to_string())));
    let ctx = Ctx::<FilterData>::new(
        InputData {
            lut: &filter.lut,
            input: &input,
        },
        Vars::new([Val::obj(Default::default())]),
    );
    let pp = write::Pp::default();
    for value in &input {
        let value =
            value.map_err(|error| anyhow::anyhow!("Invalid JSON or JSON Lines: {error}"))?;
        for result in filter.id.run((ctx.clone(), value)) {
            let value = jaq_core::unwrap_valr(result)
                .map_err(|error| anyhow::anyhow!("jq filter error: {error}"))?;
            match &value {
                Val::TStr(text) if raw => output.write_all(text.as_ref())?,
                _ => write::write(output, &pp, 0, &value)?,
            }
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
