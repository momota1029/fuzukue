use fuzukue_extension_api::{fuzukue::extension::read_files, Guest, ProjectEvent, Tool, UiEvent};
use serde_json::{json, Value};

struct MemoStats;

fn counts() -> Result<(usize, usize), String> {
    let files = read_files::read("memos/*/card.md")?;
    let words = files
        .iter()
        .map(|file| {
            let text = String::from_utf8_lossy(&file.contents);
            let body = text
                .lines()
                .skip(1)
                .skip_while(|line| *line != "---")
                .skip(1);
            body.flat_map(str::split_whitespace).count()
        })
        .sum();
    Ok((files.len(), words))
}

impl Guest for MemoStats {
    fn list_tools() -> Vec<Tool> {
        vec![Tool {
            name: "memo_stats".into(),
            description: "description".into(),
            input_schema_json: json!({"type":"object","additionalProperties":false}).to_string(),
        }]
    }

    fn invoke_tool(name: String, arguments_json: String) -> Result<String, String> {
        if name != "memo_stats"
            || serde_json::from_str::<Value>(&arguments_json).ok() != Some(json!({}))
        {
            return Err("Unknown tool or invalid arguments".into());
        }
        let (memos, words) = counts()?;
        Ok(json!({"memos": memos, "words": words}).to_string())
    }

    fn on_event(_event: ProjectEvent) -> Result<(), String> {
        Ok(())
    }

    fn render_view(_locale: String) -> Result<String, String> {
        let (memos, words) = counts()?;
        Ok(json!([
            {"type":"heading","text":{"key":"title"}},
            {"type":"table","columns":[{"key":"metric"},{"key":"count"}],
             "rows":[[{"key":"memos"},memos.to_string()],[{"key":"words"},words.to_string()]]},
            {"type":"button","id":"refresh","text":{"key":"refresh"}}
        ])
        .to_string())
    }

    fn on_ui_event(_event: UiEvent, locale: String) -> Result<String, String> {
        Self::render_view(locale)
    }
}

fuzukue_extension_api::export!(MemoStats with_types_in fuzukue_extension_api);
