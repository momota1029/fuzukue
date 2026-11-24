# カードグラフ MCP

サーバーの `labo` MCP に、ワークスペース全体のカードを読む道具がある。まず `graph_overview` でデッキと章を見て、気になるカードの近くを `graph_neighbors` や `graph_path` で辿る。

| ツール | 引数と返り値 |
| --- | --- |
| `graph_overview` | `deck?`, `depth?: 1\|2`。デッキの状態・種類別件数と章順。depth 1 は章まで（JSON 最大 6 KB）。depth 2 で章の key を chapter に渡すとカードと一行要約が掲載順に並ぶ。next_offset で続きを取得できる。 |
| `card_read` | `card`, `part?: statement\|proof\|notes\|all`, `lines?: "a-b"`。指定部位の必要な行を返す。 |
| `card_edit` | `card`, `part`, `old`, `new`。完全一致が一箇所なら置き換え、短い差分と周辺の文体・参照の検査結果を返す。 |
| `graph_neighbors` | `card`, `depth?: 1..3`, `edge_types?`, `direction?: in\|out\|both`。既定は深さ 1、両方向。ノードの一行要約と型付きの辺を返す。 |
| `graph_path` | `from`, `to`, `max_len?`。カード間の有向リンクを辿る最短経路を最大 8 本返す。既定は最大 12 辺。 |
| `graph_search` | `query`, `kinds?`, `decks?`, `limit?`。ID・題・要約・主張を重み付けして検索する。日本語は文字の二連で照合する。 |
| `graph_issues` | `deck?`。未解決の参照、`requires` の循環、構成に載らないカード、証明状態の不整合、同じデッキ内で後の章や節への参照を返す。 |
| `check_step_ledger` | `deck?`（省略時は現在のデッキ）。証明ステップ台帳を検査し、依存順、tier ごとの件数、各 key ステップの影響先を返す。 |

`deck` はワークスペースからの相対パス（ルートのデッキは `.`）。カード ID が重複するときは `<deck>#<id>` で指定する。辺の `rel` は `requires`、`links` に書いた関係（`see` など）、`from`、`mentions`（`[[id]]`）、`ref`（TeX の参照）、`cite`、`contains`。`contains` の `order` は親の中での位置を 0 から数える。

`from: lab:research/<研究>#<カード>` や `from: <名前空間>#<カード>` を辿れる。別リポジトリのチェックアウトはデッキの `[external]` に登録する。リモート URL の出典も辺として保持する。引用キーはデッキの BibTeX やワークスペースの参照資料に解決する。

`examples/lab` で `graph_overview({"deck":"books/concentration-notes","depth":2})` を呼んだときの `structuredContent` の抜粋。

```json
{
  "depth": 2,
  "decks": [{
    "deck": "books/concentration-notes",
    "kind": "book",
    "title": "『測度の集中』を噛み砕くノート",
    "status": "drafting",
    "card_count": 3,
    "counts": {"definition": 1, "expl": 1, "proposition": 1},
    "sections": [{
      "key": "section:books/concentration-notes#1",
      "title": "第1章 集中とは何か",
      "items": [
        {"id": "ch1-intro", "kind": "expl", "title": "この章で何をするか", "status": "drafted"},
        {"id": "ch1-def-median", "kind": "definition", "title": "中央値", "status": "drafted"},
        {"id": "ch1-prop-median-vs-mean", "kind": "proposition", "title": "中央値と平均の差", "status": "drafted"}
      ]
    }],
    "omitted_cards": 0
  }]
}
```

索引はサーバーの状態ディレクトリに `card-graph.json` として保存する。再起動や監視イベントの後は、更新時刻とサイズが同じファイルの解析結果を再利用する。ワークスペース外に登録したデッキの入力は MCP 呼び出し時に更新を確認する。

## 証明ステップ台帳

証明を構成する各ステップを `kind: step` のカードに記録する。`id`、`title`、`requires`、`loc`、`origin`、`tier`、`role` と、本文の `## Statement`（またはフォルダ形式の `statement.tex`）が必要になる。`requires` には同じ台帳内の前提ステップの ID を並べ、前提がない場合は `requires: []` と書く。カードの `links` に `rel: requires` として書いた依存も検査対象になる。

```markdown
---
id: estimate
kind: step
title: 境界項の評価
requires: []
loc: 補題 2.1、p. 4
origin: new
tier: key
why_tier: 結論に必要な一様評価を与える。参照した先行研究の評価は固定したパラメータに限られる。
role: 極限を取るための一様評価を与える
---

## Statement

境界項はパラメータの全範囲で $C/n$ 以下となる。
```

`origin` は `inherited`（既存の結果を引用）、`standard`（標準的）、`adapted`（既知の手法を変更）、`new`（原典が新規とする）のいずれか。`tier` は `key`、`semi`、`standard` のいずれか。`key` には必要性と先行研究との差を `why_tier` に記録する。目標の証明に使わない補足ステップには `outside: true` を指定する。

目標ステップの ID は既定で `T`。変更する場合は `deck.toml` に指定する。

```toml
[step_ledger]
target = "main-theorem"
```

`check_step_ledger` は `ok`、`errors`、`info` を返す。形式エラー、目標や依存先の欠落、循環があると `info` は `null` になる。目標から辿れないステップの指摘があっても `info` を返す。`info` の `order` は前提を先に並べた順序、`key_impact` は各 `key` ステップに直接または間接に依存する全ステップ、`tiers` は目標を除いた件数、`target` は目標 ID を表す。

資料の取得状況は[取得台帳](ledger.md)で管理する。

## 文体の指摘
ワークスペースの `fuzukue.toml` で `[style_lint].enabled = false` にすると文体検査を無効にできる（既定は `true`）。`[style_lint].exclude` にはワークスペース相対の glob を追加できる。
`cards/design/**`、`memos/**`、`inbox/**`、`threads/**` と `plan`、`memo`、`design`、`design-log` のカードは既定で対象外。指摘はカードのファイル情報の下で折りたたまれ、開いた行を選ぶと該当箇所へ移動する。

## 節の数学査読で辿る参照

`request_review` に `cards` を渡すと、配列の順序で対象を固定する。数学査読の写しには、対象カードから辿れるカードの依存辺、TeX のラベル参照、リテラルで指定された入力ファイルが入る。ラベルの抽出はグラフと共有し、`\ref` や `\eqref` に加えて `\nameref`、`\pageref`、`\vref` も認識する。解決できない参照や入力は査読時の資料に記録する。指摘を付けられるのは依頼時に指定したカードだけ。
