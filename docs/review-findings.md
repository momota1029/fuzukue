# 証明・原稿の指摘台帳

デッキ直下の `review-findings.toml` に、証明・原稿の確認で追うと決めた指摘を 1 件ずつ保存する。人が見つけた指摘と、エージェントのレビューから採用した指摘を同じ形式で扱う。手入力の登録、一覧、更新には MCP を使う。カードとデッキから未対応件数を確認し、台帳を開ける。

## MCP で手入力する

受付に指摘の内容と出どころを伝えて登録するか、MCP クライアントから次のツールを呼ぶ。すべてのツールで `deck` はワークスペース相対のパス。省略時は現在のデッキ、ルートデッキを明示する場合は `.` を指定する。

| ツール | 入力と結果 |
| --- | --- |
| `record_review_finding` | `id` に新しい UUID（小文字）、`actor` に記録者、`finding` に内容を渡す。初期状態は `open`。同じ登録を再送するときは同じ UUID を使う。 |
| `list_review_findings` | デッキの全件を重さ順で返す。各指摘には状態、履歴、更新用の `revision` が付く。デッキ全体の `open_count` とカードごとの `card_open_counts` も返す。 |
| `update_review_finding` | `id` と直近に読んだ `expected_revision` を渡す。内容を更新する場合は `finding` に登録時と同じ形式で差し替え後の全項目を渡す。状態を変える場合は `transition` に遷移先の `status`、記録者の `actor` と、必要な `reason` または `changes` を渡す。 |

たとえば `record_review_finding` に次の引数を渡す。エージェントの指摘には出どころの `kind` を `agent` とし、分かる場合は `perspective`、`thread`、`review` を添える。元の指摘 ID を `finding` に含める場合は `review` も指定する。

```json
{
  "deck": "papers/draft",
  "id": "cb38ac8b-d670-47be-91e3-9c4ecdb61d26",
  "actor": "editor",
  "finding": {
    "summary": "補題の仮定を補う",
    "detail": "証明で列の有界性を使うが、補題に仮定が書かれていない。",
    "severity": "major",
    "locations": [{"card": "lemma", "file": "papers/draft/Proof.lean", "lines": [12, 14]}],
    "sources": [{"kind": "human", "reviewer": "reader"}]
  }
}
```

`fixed` にするとき、`transition.changes` の各要素に `summary`、`locations`、`reference` を渡す。Git では `reference` に `kind: "commit"` と完全な `oid`、カード編集では `kind: "card_edit"` と `record`（変更履歴レコード ID）、`card`（カード ID）を入れる。参照は記入された識別子で保存するため、履歴を別端末から取得できない場合も箇所と説明を読める。見送りと再開には `reason` を渡す。

## 現行の査読と確認ツール

外部査読は `papers/` 配下のデッキを対象とし、`<deck>/submissions/<id>/` に `submission.toml`、`reports/<番号>.md`、`points.toml`、必要に応じて `response.md` を置く。投稿には投稿先・投稿日・投稿時の版が必要。原文を保存し、分割案を利用者が確定すると、原文の行範囲と引用を持つ `R<報告番号>.<連番>` が `points.toml` に入る。ここでの行範囲は査読報告を指す。修正先は `fixed_locations` の文字列として別に記録する。返答書は確認済みのポイントの引用・返答・修正先を並べて生成する。 [保存先](review-reports.md) · [確定処理](../crates/secretaire/src/review_reports.rs#L266) · [返答書の生成](../crates/secretaire/src/review_reports.rs#L337)

| 現行ツール | サーバーでの処理 |
| --- | --- |
| `list_review_submissions {deck}` | 投稿を読み、投稿 ID とメタデータ、報告番号、確定したポイント ID を返す。 |
| `read_review_point {deck, submission, point}` | 指定の投稿の `points.toml` を読む。引用と要約、関係カード、状態、返答、修正先、設定済みなら対応スレッドを返す。 |
| `update_review_point {deck, submission, point, …}` | 関係カード、状態、返答、修正先のうち渡された項目だけを上書きする。対応スレッドの実行中は拒否する。状態には `todo`、`in_progress`、`fixed`、`rebut`、`wont_fix` を使い、`fixed` へ変えるとき修正先は必須でない。ファイルを原子的に置き換え、査読欄に新しいスナップショットを配信する。 |

[MCP の実装](../crates/secretaire/src/mcp.rs#L1876) · [ポイントの型](../crates/fuzukue-proto/src/review_reports.rs#L25) · [ファイル更新](../crates/secretaire/src/review_reports.rs#L249) · [更新と配信](../crates/secretaire/src/review_reports.rs#L584)

論文ビューの「査読」は WebSocket で同じ投稿を読み、投稿ごとのポイントを状態で絞り込む。論文のカード表示では、`point.cards` に当該 ID があるポイントを状態にかかわらず表示し、マーカーから投稿内のポイント詳細へ移る。資料庫の査読検索も同じ詳細を開く。「直す」は対応スレッドを起動して `in_progress` にし、終了後に利用者が状態を更新する。通常のカード詳細とデッキのカード一覧には、査読ポイントの件数を出す入口がない。別経路の `request_review` はレビュー結果をコメントの下書きに追加する。 [査読欄](../crates/fuzukue/src/paper.rs#L909) · [カードのマーカー](../crates/fuzukue/src/review_reports.rs#L518) · [状態フィルター](../crates/fuzukue/src/review_reports.rs#L844) · [資料庫](../crates/fuzukue/src/library.rs#L254) · [対応スレッド](../crates/secretaire/src/review_reports.rs#L518) · [確認から下書きへの経路](../crates/secretaire/src/reviewers.rs#L701)


## 保存方式の比較

| 案 | 利点 | 実装上の負担 |
| --- | --- | --- |
| `ReviewPoint` を拡張する | 既存の保存・更新、カードへの逆引き、一覧と詳細を活用できる。 | 現在の形で保存すると内部の確認にも投稿、報告、引用が必要になる。出典をタグ付きの型に作り替えるなら、`deck + submission + point` の MCP 契約、資料庫からのリンク、返答書の生成対象、既存ファイルの読み込みを一緒に設計する必要がある。既存の状態だけでは確度と対応の両方を表せない。 |
| デッキごとに指摘台帳を持つ | 投稿前の原稿や Lean の証明、カードに未整理の原稿ファイルをそのデッキで追える。根拠と対応履歴を項目ごとに記録できる。 | 台帳の読み書きと詳細への導線が必要。外部査読との共通一覧を将来作る場合は、両方を読み取る表示が必要になる。 |

デッキごとの台帳を採用する。デッキの解決、原子的なファイル更新、カード・ソース・変更履歴へのナビゲーションは既存の機構を使う。査読ポイントと返答書は現行の経路で管理する。既存データを移す作業は不要。取得台帳 `references/ledger.toml` は資料の取得状況を扱っている。 [デッキの解決](../crates/secretaire/src/workspace.rs#L352) · [原子的な更新](../crates/secretaire/src/references.rs#L544) · [取得台帳](ledger.md)

## データモデル

`<deck>/review-findings.toml` は `version = 1` と `[[findings]]` を持つ。ルートデッキならルート直下に置く。デッキ内の件数表示と更新を同じファイルから行い、Git に入れて履歴を共有できる。ファイルがまだなければ空の台帳として読み、最初の保存で作る。

| 項目 | 内容 |
| --- | --- |
| `id`、`summary`、`detail` | デッキ内で一意な UUID と、指摘の要約・詳細。ID はカードの移動、行ずれ、状態変更や再開で変えない。参照にはデッキの相対パスと ID を使う。 |
| `locations` | 1 件以上。各要素は所有デッキ内の `card`、ワークスペース相対の `file`、または両方を持つ。`file` には任意で 1 始まり・両端を含む `lines = [開始, 終了]` と確認時の `revision` を添えられる。カード関連付けは明示し、ファイル名から推定しない。同じ指摘を複数カードに関連付けられる。 |
| `severity` | `fatal`（致命的）、`major`（重大）、`minor`（軽微）、`form`（体裁）。 |
| `sources` | 1 件以上。人の確認、またはエージェントのレビューを記録し、分かる範囲で確認者・観点・元のスレッドやレビュー識別子を添える。同じ指摘を複数の観点が挙げた場合は一つのレコードに併記する。 |
| `assessment`、`evidence` | 任意の確度（`confirmed`、`suspected`、`unchecked`）と検証根拠。確度を `confirmed` とするなら根拠を添える。レビューでの裁定がなければ省略できる。 |
| `transitions` | 作成・対応・見送り・再開の順序付き履歴。各操作に記録者、時刻、遷移先を持たせる。現在の状態は最後の記録から求める。 |

`fixed` の記録には 1 件以上の変更参照を保存する。Git の場合は修正コミットの完全な OID、カード編集の場合は変更履歴レコード ID とカード ID を保存し、変更箇所と短い説明を添える。カード変更履歴が別端末などで取得できなくても、保存した識別子・箇所・説明は読めるようにする。同じコミットで複数件を閉じた場合も、どの指摘を閉じたかが各レコードから分かる。 [変更履歴の型](../crates/fuzukue-proto/src/change_history.rs#L36) · [カード単位の履歴と差分](../crates/secretaire/src/history.rs#L1189)

## 状態遷移

| 現在 | 操作 | 次の状態 | 条件・記録 |
| --- | --- | --- | --- |
| 作成前 | 指摘を登録 | `open`（未対応） | 場所、重さ、内容、出どころを保存する。レビュー下書きからは、利用者が追うと決めた指摘を登録する。 |
| `open` | 対応済みにする | `fixed`（対応済み） | 修正を確認し、閉じた変更を添えて登録する。 |
| `open` | 見送る | `deferred`（見送り） | 理由を記録する。 |
| `fixed`、`deferred` | 再開する | `open`（未対応） | 再開理由を記録し、以前の変更参照や見送り理由を履歴に残す。 |

状態とその変更参照・理由は一度に保存する。対応中の作業も閉じるまでは `open` として数える。対応の終了や原稿の差分を検出しても状態は自動更新しない。閉じた状態を変えるときは、既存の記録を残して再開してから処理する。

MCP の更新では書き込みを直列化し、読み直した台帳で編集中のレコードが変更されていれば再読み込みを求める。同じ作成要求の再送では同じ ID を使う。更新前にファイル全体と遷移を検証する。Git checkout や手での編集は再読込時に反映し、読み取れないファイルや重複 ID は読み込みエラーとして返す。MCP での保存に成功すると、同じデッキを表示している画面に新しい台帳が届く。ファイルへのリンクを開くときはワークスペース内に解決できることを検証する。

## カードとデッキの表示

| 画面 | 件数と遷移先 |
| --- | --- |
| 通常のカード詳細と論文のカード表示 | カード ID が `locations` にある `open` の指摘を ID 単位で数える。「未対応の指摘 N 件」から、そのカードで絞り込んだ台帳を開く。閉じた指摘だけが残るカードでは「指摘の履歴」を出す。 |
| デッキ概要・カード一覧、論文ビューのツールバー | デッキの「未対応の指摘 N 件」からデッキ全体を開く。0 件でも開けるため、最初の登録と閉じた指摘の確認に使える。同じ指摘が複数のカードにあっても合計は 1 件。カード未指定の指摘や、カードが移動・削除された指摘も合計に含む。カード一覧の行には当該カードの未対応件数があるときに表示する。 |
| 台帳一覧・詳細 | 初期表示は未対応。対応済み・見送り・全件へ切り替えられる。一覧は重さ順（`fatal` → `form`、同じ重さは ID 順）で、要約、場所、重さ、状態を表示する。詳細で出どころ、確度と根拠、状態履歴、閉じた変更と見送り理由を読める。カードから開いた場合は一覧に戻ってもカードの絞り込みを保つ。 |

カードやファイルを開けるときは既存の遷移を使う。確認時の版が現在と違う場合は保存した行番号にその版を添え、現在の行として扱わない。カードやファイルが見つからない場合も参照先の文字列は表示し、MCP で関連付けを更新できる。デッキ切り替え時はデッキのパスでスナップショットを照合し、読込中やエラーを 0 件と表示しない。 [通常のカード詳細](../crates/fuzukue/src/main.rs#L2778) · [概要への入口](../crates/fuzukue/src/main.rs#L2446) · [デッキのカード一覧](../crates/fuzukue/src/deck_overview.rs#L157) · [論文のカード表示](../crates/fuzukue/src/paper.rs#L727)

## 手入力と画面の確認例

手入力での登録と状態変更は MCP で行う。デスクトップでの表示と再読み込みには v63 の `ReviewFindings` イベントを使う。`findings.py` の JSON 一括取り込みと既存のコメント下書きからの登録は後続の接続候補とする。現行の査読 MCP と返答書には変更を要しない。

確認例として、2 枚のカードに付いた指摘とファイルだけに付いた指摘を各 1 件登録する。デッキでは未対応 2 件、各カードでは未対応 1 件を表示する。カードに付いた指摘を変更参照付きで閉じると、デッキでは未対応 1 件になり、両カードの履歴から同じ変更を開ける。再開後は件数が戻り、閉じた変更の記録も残る。見送り、台帳がないデッキ、壊れた台帳、別デッキへの切り替え、既存の外部査読マーカーと返答書生成も確認する。Linux のヘッドレスシナリオ `tests/shots/review-findings.json` で件数と台帳への遷移を確認できる。 [ヘッドレス検証](headless.md)


## 節のブラインド査読から登録する

`read_section_review` で初回の受領記録を読み、編集者が返答の根拠・引用・位置を照合する。明示した採否は `POST /section-review/editor` に JSON で送る。接続先の編集者の端末トークンまたは共有トークンを `Authorization: Bearer …` に指定する。別ワークスペースはクエリの `workspace` で指定する。

| `action` | JSON の項目 |
| --- | --- |
| `decide` | 初回の `session`、返答内の `finding` ID、受領記録の `reviewed_version` と `input_hash`、`choice`（`accept`／`reject`／`hold`）、`reason`。同じ主張と位置を追う既存の行に出典を追記する場合は `append_to: {id, expected_revision}` も指定する。 |
| `fix` | 初回の `session`、返答内の `finding` ID、新版の `recheck_session`、新版の `reviewed_version`、完全な `commit` OID、該当する `checked: [{card, location: {excerpt, lines}, criterion}]`、台帳の `expected_revision`、`reason`。 |
| `defer` | 初回の `session`、返答内の `finding` ID、台帳の `expected_revision`、`reason`。 |
| `stop` | 初回の `session`、`reason`（`approval_target_unmet`／`budget_stop`／`user_stop`）、`detail`。 |

受付は採用された指摘を `register_section_review_finding {session, finding}` で一件ずつ登録する。出典の追記や修正確認で台帳の版が競合したら、`list_review_findings` で読み直して編集者が照合する。

修正を Git に保存した後は、受付がブラインドの `request_review` に `recheck: {initial_session, commit, affected_uses?}` を付ける。初回と同じデッキ・節・提供元・モデルを使い、初回の対象と、節内で影響を受ける使用先を掲載順に指定する。既知の使用先はカードの依存と TeX の参照からも探す。新版で変わった対象の行と、引き続き確認が必要な行を許可範囲に含める。再送には同じ引数を使う。初回と新版の実行、採否、残件、終了理由は `read_section_review` の `workflow` から確認できる。
