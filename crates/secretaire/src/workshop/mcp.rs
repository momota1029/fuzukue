use std::sync::Arc;

use anyhow::{Context, Result};
use fuzukue_proto::Actor;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::Workspace;

use super::{Create, Kind, State};

pub(crate) fn prompt() -> &'static str {
    harness_text!(
        "\nUse the research_issue_* MCP tools for research issues. research_issue_list gives the repository for the current workspace and the issue bodies and comments. Use that result to identify issues by number.",
        "\n研究 Issue は research_issue_* の MCP ツールで扱う。research_issue_list は現在のワークスペースのリポジトリと Issue の本文・コメントを返す。番号で指定された Issue はその一覧で特定する。",
    )
}

#[derive(Clone, Copy)]
pub enum Tool {
    Create,
    List,
    Start,
    Accept,
}

impl Tool {
    pub const ALL: [Self; 4] = [Self::Create, Self::List, Self::Start, Self::Accept];

    pub fn name(self) -> &'static str {
        match self {
            Self::Create => "research_issue_create",
            Self::List => "research_issue_list",
            Self::Start => "research_issue_start",
            Self::Accept => "research_issue_accept",
        }
    }

    pub fn writes(self) -> bool {
        !matches!(self, Self::List)
    }

    pub fn definition(self) -> Value {
        let kind = json!({"type":"string","enum":["lean","proof","literature","check","other"]});
        let number = json!({"number":{"type":"integer","minimum":1}});
        let (description, properties, required): (_, _, &[&str]) = match self {
            Self::Create => (harness_text!("Wait for user approval, create an issue in the research repository, and link it from the entry cards.", "利用者の承認を待って研究リポジトリに Issue を作り、入口カードにもリンクを残す。"),
                json!({"title":{"type":"string"}, "cards":{"type":"array","minItems":1,
                    "items":{"type":"string","description":harness_text!("Workspace or repository:deck#card.", "ワークスペースまたはリポジトリ:デッキ#カード")}},
                    "kind":kind, "acceptance":{"type":"string"},
                    "assignee":{"type":"string","description":harness_text!("local, dots, or a worker model name.", "local、dots、または作業役モデル名")},
                    "producer":{"type":"string","description":harness_text!("Model that produced the work being verified.", "検証対象を作ったモデル名")}}),
                &["title", "cards", "kind", "acceptance"]),
            Self::List => (harness_text!("List the current workspace’s research issues with their repository, status, entry cards, body, and comments.", "現在のワークスペースの研究 Issue を、リポジトリ・状態・入口カード・本文・コメント付きで返す。"),
                json!({"kind":kind,"state":{"type":"string","enum":["ready","running","needs_review","incomplete","done"]}}), &[]),
            Self::Start => (harness_text!("Gather entry card context and delegate in a new thread. Queue dots after approval.", "入口カードの文脈を集め、新規スレッドで作業役に委譲する。dots は承認後にキューへ出す。"), number, &["number"]),
            Self::Accept => (harness_text!("Wait for user approval to accept the work, comment on GitHub, and close the issue.", "成果の取り込み、GitHub へのコメントとクローズについて利用者の承認を待つ。"), number, &["number"]),
        };
        json!({"name":self.name(),"description":description,"inputSchema":{
            "type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }

    pub async fn call(
        self,
        caller: &Arc<Workspace>,
        session: &str,
        actor: Actor,
        args: &Value,
    ) -> Result<Value> {
        let registry = caller
            .registry
            .upgrade()
            .context("ワークスペースの一覧が利用できない")?;
        match self {
            Self::Create => {
                super::create(
                    &registry,
                    caller,
                    session,
                    actor,
                    Create::deserialize(args)?,
                )
                .await
            }
            Self::List => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Filter {
                    state: Option<State>,
                    kind: Option<Kind>,
                }
                let Filter { state, kind } = Filter::deserialize(args)?;
                Ok(
                    json!({"repo": caller.research_issues.repo()?, "issues": super::list(&registry, caller, state, kind, false).await?}),
                )
            }
            Self::Start | Self::Accept => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Number {
                    number: u64,
                }
                let Number { number } = Number::deserialize(args)?;
                anyhow::ensure!(number > 0, "Issue 番号は 1 以上を指定する");
                if matches!(self, Self::Start) {
                    Ok(
                        json!({"session": super::start(&registry, caller, session, actor, number).await?}),
                    )
                } else {
                    super::accept(&registry, caller, session, actor, number).await?;
                    Ok(json!({"number":number, "state":"done"}))
                }
            }
        }
    }
}
