use std::collections::{BTreeMap, HashSet};

use anyhow::{ensure, Result};
use fuzukue_proto::{Question, QuestionAnswer, QuestionOption, SECRET_ANSWER_MASK};
use serde::Deserialize;
use serde_json::{json, Value};

pub(crate) type DisplayAnswers = BTreeMap<String, Vec<String>>;

pub(crate) struct ValidatedAnswer {
    labels: Vec<String>,
    text: Option<String>,
}

pub(crate) type ValidatedAnswers = BTreeMap<String, ValidatedAnswer>;
pub(crate) type QuestionReply = Option<ValidatedAnswers>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingQuestion {
    id: Option<String>,
    header: Option<String>,
    question: String,
    options: Option<Vec<IncomingOption>>,
    #[serde(default)]
    multi_select: bool,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
}

#[derive(Deserialize)]
struct IncomingOption {
    label: String,
    description: Option<String>,
}

pub(crate) fn parse(input: &Value, claude: bool) -> Result<Vec<Question>> {
    let incoming: Vec<IncomingQuestion> = serde_json::from_value(input["questions"].clone())?;
    ensure!(!incoming.is_empty(), "No questions supplied");
    let mut keys = HashSet::new();
    incoming
        .into_iter()
        .enumerate()
        .map(|(index, question)| {
            let options = question
                .options
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(index, option)| QuestionOption {
                    id: index.to_string(),
                    label: option.label,
                    description: option.description,
                })
                .collect::<Vec<_>>();
            let id = if claude {
                index.to_string()
            } else {
                question.id.unwrap_or_default()
            };
            let key = if claude { &question.question } else { &id };
            ensure!(
                !key.trim().is_empty() && keys.insert(key.clone()),
                "Invalid question key"
            );
            ensure!(!question.question.trim().is_empty(), "Empty question");
            ensure!(
                options.iter().all(|option| !option.label.trim().is_empty()),
                "Empty option label"
            );
            Ok(Question {
                id,
                header: question.header,
                question: question.question,
                multi_select: claude && question.multi_select,
                allow_other: claude || question.is_other || options.is_empty(),
                secret: question.is_secret,
                options,
            })
        })
        .collect()
}

pub(crate) fn validate(
    questions: &[Question],
    answers: &BTreeMap<String, QuestionAnswer>,
) -> Result<ValidatedAnswers> {
    ensure!(answers.len() == questions.len(), "Answer every question");
    questions
        .iter()
        .map(|question| {
            let answer = answers
                .get(&question.id)
                .ok_or_else(|| anyhow::anyhow!("Missing answer"))?;
            let text = answer
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty());
            ensure!(
                text.is_none() || question.allow_other,
                "Free text is unavailable for this question"
            );
            let count = answer.selected.len() + usize::from(text.is_some());
            ensure!(count > 0, "Empty answer");
            ensure!(question.multi_select || count == 1, "Choose one answer");
            let mut seen = HashSet::new();
            let labels = answer
                .selected
                .iter()
                .map(|id| {
                    ensure!(seen.insert(id), "Duplicate option");
                    let option = question
                        .options
                        .iter()
                        .find(|option| option.id == *id)
                        .ok_or_else(|| anyhow::anyhow!("Unknown option"))?;
                    Ok(option.label.clone())
                })
                .collect::<Result<_>>()?;
            Ok((
                question.id.clone(),
                ValidatedAnswer {
                    labels,
                    text: text.map(str::to_owned),
                },
            ))
        })
        .collect()
}

pub(crate) fn display(questions: &[Question], answers: &ValidatedAnswers) -> DisplayAnswers {
    questions
        .iter()
        .map(|question| {
            let answer = &answers[&question.id];
            let values = if question.secret {
                vec![SECRET_ANSWER_MASK.into()]
            } else {
                answer
                    .labels
                    .iter()
                    .chain(answer.text.iter())
                    .cloned()
                    .collect()
            };
            (question.id.clone(), values)
        })
        .collect()
}

pub(crate) fn claude_response(
    request_id: &str,
    mut input: Value,
    questions: &[Question],
    reply: QuestionReply,
) -> Value {
    // The SDK preserves updatedInput verbatim. Anthropic documents comma-joined labels
    // and uses that form in its TypeScript example.
    let response = if let Some(answers) = reply {
        let answers: BTreeMap<_, _> = questions
            .iter()
            .map(|question| {
                let answer = &answers[&question.id];
                let values = answer
                    .labels
                    .iter()
                    .chain(answer.text.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                (&question.question, values.join(", "))
            })
            .collect();
        input["answers"] = json!(answers);
        json!({ "behavior": "allow", "updatedInput": input })
    } else {
        json!({ "behavior": "deny", "message": "The user dismissed the question." })
    };
    json!({ "type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": response
    } })
}

pub(crate) fn codex_response(id: Value, reply: QuestionReply) -> Value {
    // Codex 0.169.0's app-server test client marks typed input as user_note.
    // The backend also uses an empty map when a client declines to provide answers.
    let answers: BTreeMap<_, _> = reply
        .unwrap_or_default()
        .into_iter()
        .map(|(id, answer)| {
            let mut values = answer.labels;
            values.extend(answer.text.map(|text| format!("user_note: {text}")));
            (id, json!({ "answers": values }))
        })
        .collect();
    json!({ "jsonrpc": "2.0", "id": id, "result": { "answers": answers } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_preserves_input_and_keys_answers_by_question_text() -> Result<()> {
        let input = json!({
            "metadata": {"source": "fixture"},
            "questions": [
                {"question": "Format?", "options": [{"label": "Summary"}]},
                {"question": "Sections?", "multiSelect": true, "options": [{"label": "Intro"}, {"label": "End"}]},
                {"question": "Notes?", "options": [{"label": "Default"}]}
            ]
        });
        let questions = parse(&input, true)?;
        let answers = serde_json::from_value(json!({
            "0": {"selected": ["0"]}, "1": {"selected": ["0", "1"]}, "2": {"text": "Custom"}
        }))?;
        let reply = claude_response(
            "control-id",
            input.clone(),
            &questions,
            Some(validate(&questions, &answers)?),
        );
        assert_eq!(reply["response"]["request_id"], "control-id");
        assert_eq!(reply["response"]["response"]["behavior"], "allow");
        let updated = &reply["response"]["response"]["updatedInput"];
        assert_eq!(updated["metadata"], input["metadata"]);
        assert_eq!(updated["questions"], input["questions"]);
        assert_eq!(
            updated["answers"],
            json!({"Format?": "Summary", "Sections?": "Intro, End", "Notes?": "Custom"})
        );
        let dismissed = claude_response("control-id", input, &questions, None);
        assert_eq!(
            dismissed["response"]["response"],
            json!({"behavior": "deny", "message": "The user dismissed the question."})
        );
        Ok(())
    }

    #[test]
    fn codex_preserves_rpc_id_and_accepts_other_and_nullable_options() -> Result<()> {
        let questions = parse(
            &json!({"questions": [
                {"id": "one", "question": "Choice?", "options": [{"label": "First", "description": "Details"}]},
                {"id": "two", "question": "Other?", "options": [{"label": "Default"}], "isOther": true},
                {"id": "secret", "question": "Value?", "options": null, "isSecret": true}
            ]}),
            false,
        )?;
        let answers = serde_json::from_value(json!({
            "one": {"selected": ["0"]}, "two": {"text": "Different"}, "secret": {"text": "private"}
        }))?;
        let answers = validate(&questions, &answers)?;
        assert_eq!(
            display(&questions, &answers)["secret"],
            [SECRET_ANSWER_MASK]
        );
        assert_eq!(
            codex_response(json!("rpc-id"), Some(answers)),
            json!({
                "jsonrpc": "2.0", "id": "rpc-id", "result": {"answers": {
                    "one": {"answers": ["First"]}, "two": {"answers": ["user_note: Different"]},
                    "secret": {"answers": ["user_note: private"]}
                }}
            })
        );
        assert_eq!(
            codex_response(json!(12), None),
            json!({"jsonrpc":"2.0", "id":12, "result":{"answers":{}}})
        );
        Ok(())
    }

    #[test]
    fn validates_ids_cardinality_and_nonempty_text() -> Result<()> {
        let mut questions = parse(
            &json!({"questions": [
                {"id": "q", "question": "Choose?", "options": [{"label": "A"}, {"label": "B"}]}
            ]}),
            false,
        )?;
        for invalid in [
            json!({}),
            json!({"wrong": {"selected": ["0"]}}),
            json!({"q": {"selected": ["bogus"]}}),
            json!({"q": {"selected": ["0", "1"]}}),
            json!({"q": {"text": "  "}}),
            json!({"q": {"text": "unavailable"}}),
            json!({"q": {"selected": ["0"], "text": "extra"}}),
        ] {
            assert!(validate(&questions, &serde_json::from_value(invalid)?).is_err());
        }
        questions[0].multi_select = true;
        questions[0].allow_other = true;
        assert!(validate(
            &questions,
            &serde_json::from_value(json!({"q": {"selected": ["0", "0"]}}))?
        )
        .is_err());
        assert!(validate(
            &questions,
            &serde_json::from_value(json!({"q": {"selected": ["0", "1"], "text": "extra"}}))?
        )
        .is_ok());
        Ok(())
    }
}
