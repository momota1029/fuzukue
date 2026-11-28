# 会話の Markdown に XML UI 部品を埋め込む

返信本文の行頭に `<fuzukue:…>` で始まる XML 要素を一つ置くと、対応するデスクトップクライアントが部品として表示します。前後を空行で区切り、閉じタグの行には続けて文章を書かないでください。返信の先頭や末尾にも置けます。比較、分岐、進捗など、構造が理解を助けるときに使い、普段の返信は普通の Markdown で書きます。

受信中は終端の行が確定するまで小さな待機表示を出します。返信が終わった時点で解釈できない入力は元のテキストを表示します。既存の `~~~fuzukue-v1` フェンスも引き続き使えます。

## 進捗カードの例

````markdown
### I-66：残りの証明を数えられるようになった

`proof_status` で I-65 Step 1 の残りを一覧にできる。

<fuzukue:card xmlns:fuzukue="urn:fuzukue:ui:v1" title="I-65 Step 1の証明状況">
  <fuzukue:grid columns="2">
    <fuzukue:stat label="部分証明あり" value="10" unit="件" tone="warn"/>
    <fuzukue:stat label="未証明" value="3" unit="件"/>
  </fuzukue:grid>
  <fuzukue:progress value="10" max="13" tone="warn" caption="13件のうち10件に部分証明あり（77%）"/>
  <fuzukue:divider/>
  <fuzukue:text style="heading">完全に未証明の3件</fuzukue:text>
  <fuzukue:list>
    <fuzukue:item>S01：一様有界性</fuzukue:item>
    <fuzukue:item>S05：誤差項の評価</fuzukue:item>
    <fuzukue:item>S09：極限と積分の交換</fuzukue:item>
  </fuzukue:list>
</fuzukue:card>

部分証明のある10件は、それぞれの残りの仮定を確認する。

OPEN：S01、S05、S09 の証明方針を決める。
````

## 手順とチェックリストの例

````markdown
<fuzukue:card title="原稿を確認する" tone="info">
  <fuzukue:text><fuzukue:badge tone="info">草稿</fuzukue:badge> を開き、<fuzukue:kbd>Enter</fuzukue:kbd> で <fuzukue:code>proof_status</fuzukue:code> を実行します。</fuzukue:text>
  <fuzukue:steps>
    <fuzukue:step title="仮定を照合" status="done"/>
    <fuzukue:step title="証明を確認" status="current">残りの評価を読みます。</fuzukue:step>
    <fuzukue:step title="結果を記録"/>
  </fuzukue:steps>
  <fuzukue:details title="記録前の確認" open="false">
    <fuzukue:list kind="checklist">
      <fuzukue:item checked="true">仮定を一覧にした</fuzukue:item>
      <fuzukue:item checked="false">評価の参照を付けた</fuzukue:item>
    </fuzukue:list>
  </fuzukue:details>
  <fuzukue:progress percent="60" caption="全体の60%まで確認">
    <fuzukue:segment value="40" tone="success"/>
    <fuzukue:segment value="20" tone="info"/>
  </fuzukue:progress>
</fuzukue:card>

次は評価の参照を付けます。
````

## 分岐付きフローチャートの例

````markdown
<fuzukue:flowchart>
  <fuzukue:step label="原稿を確認する"/>
  <fuzukue:decision question="すべての確認が終わった？">
    <fuzukue:branch label="はい" tone="success">原稿を公開する</fuzukue:branch>
    <fuzukue:branch label="いいえ" tone="warn">残りを確認してから見直す</fuzukue:branch>
  </fuzukue:decision>
</fuzukue:flowchart>
````

手順と判断を記載順に上から並べます。判断の直下に二つ以上の分岐を置き、それぞれの条件と結果を表示します。幅の狭い画面でも縦に並べ、長いラベルや結果は折り返します。

## コールアウト・表・指摘参照の例

MCP ツールの結果を使う場合も、返信本文に同じ記法を書きます。`finding` の `deck` はプロジェクト相対のデッキパス（ルートなら `.`）、`id` は指摘台帳にある小文字の UUID です。たとえば `list_review_findings` が返した実在する ID を指定します。クライアントは開いているデッキの読み込み済みスナップショットを参照し、該当する指摘が見つからない場合もパスと ID を表示します。

````markdown
~~~fuzukue-v1
<callout severity="warn" title="仮定の確認">
定数が因子の個数に依存しないことを確かめます。
</callout>
~~~

~~~fuzukue-v1
<fuzukue:table>
  <fuzukue:row><fuzukue:cell>確認項目</fuzukue:cell><fuzukue:cell>状態</fuzukue:cell></fuzukue:row>
  <fuzukue:row><fuzukue:cell>定理の仮定</fuzukue:cell><fuzukue:cell>確認済み</fuzukue:cell></fuzukue:row>
  <fuzukue:row><fuzukue:cell>一様な評価</fuzukue:cell><fuzukue:cell>要確認</fuzukue:cell></fuzukue:row>
</fuzukue:table>
~~~

~~~fuzukue-v1
<fuzukue:finding deck="research/concentration" id="cb38ac8b-d670-47be-91e3-9c4ecdb61d26"/>
~~~

台帳の指摘では一様性の明記を求めています。
````

## 語彙

以下の新しい名前は `fuzukue:` を付けて書きます。既存の `callout`、`table`、`finding` は接頭辞なしでも使えます。入れ子にした部品は記載順に表示します。

| 要素 | 属性と子要素 |
| --- | --- |
| `card` | `title` と任意の `tone`。本文にテキストと部品を置けます。 |
| `grid` | `columns` は 1〜4。子を同じ幅で並べ、部品の幅に列数分の余裕がなければ 1 列にします。 |
| `stat` | `label` と `value`、任意の `unit`、`tone`、`hint`。単位は値の直後に表示します。 |
| `progress` | `value` と正の `max`、または 0〜100 の `percent`。任意の `label`、`caption`、`tone`。任意の `segment` を左から並べます。各区間の正の `value` は全体と同じ単位で、`tone` が必要です。区間の合計は完了値以下で、残る完了分をバー本体のトーンで描きます。 |
| `text` | `style="body"`（既定）または `style="heading"`。テキスト、`badge`、`kbd`、`code` を同じ行の流れに置けます。 |
| `badge`、`kbd`、`code` | 短い一行のテキスト。バッジには任意の `tone`。キー名とコードには等幅書体を使います。XML 外の通常の Markdown ではバッククォートでインラインコードを書けます。 |
| `list` | `kind="unordered"`（既定）、`ordered`、`checklist` のいずれか。各 `item` にテキストと部品を置けます。チェックリストの `item` は `checked="true"` または `"false"`（既定）を指定できます。 |
| `steps` | 一つ以上の `step`。各手順に `title` と任意の `status`（`pending` が既定、`current`、`done`）、本文にテキストと部品を置けます。 |
| `flowchart` | 一つ以上の `step` や `decision`。フローチャートの `step` は `label` と任意の `tone` を持つ空要素です。`decision` に `question` と二つ以上の `branch` を置きます。各 `branch` に `label`、任意の `tone`、一行の結果を書きます。 |
| `details`、`accordion` | 同じ折りたたみ部品です。`title` と任意の `open="true"` または `"false"`（既定）。本文にテキストと部品を置けます。ボタンをクリックするか、Tab で移動して Enter や Space を押すと開閉できます。 |
| `divider` | 区切り線です。 |
| `callout` | `severity` は `info`、`warn`、`error` のいずれか。`title` と本文を置きます。従来の接頭辞なしの書き方は本文をプレーンテキストとして扱います。 |
| `table` | `row` と `cell` を使います。先頭行が見出しで、セルにはプレーンテキストを置きます。各行のセル数を揃えます。 |
| `finding` | `deck` と `id` で既存の指摘を参照します。 |

`tone` は `info`、`success`、`warn`、`error`、`accent`、`muted` のいずれかです。実際の色は現在のテーマのトークンから取り、色以外にも見出し、値、チェック記号などを表示します。

## 名前空間とフェンス

新しい部品は `fuzukue:` 接頭辞を使い、最上位の要素に `xmlns:fuzukue="urn:fuzukue:ui:v1"` を任意で指定できます。この URI は語彙と版を識別します。名前空間を分けると、将来 `<trpg:dice>` のような別の語彙が加わっても意味を区別できます。

CommonMark は `<fuzukue:table>` を自動リンクとして解釈します。そのため、クライアントは通常の Markdown を組み立てる前に部品の範囲を取り出します。対象は列 1 から始まる最上位ブロックです。インラインコード、フェンス付きコード、字下げコード、引用、リストの内部は通常の Markdown として扱います。引用符 `>` のない継続行も、CommonMark 上で引用内なら対象外です。旧形式のフェンス内では従来どおり `callout`・`table`・`finding` の接頭辞を省略できます。

対応クライアントは `Hello.markdown_components` で `{"tag":"reply-blocks","version":1}` を通知します。その接続から受付への直接の会話では、Claude と Codex に短い記法案内が付きます。作業役への依頼や、ファイル、GitHub、メールなどに出す内容には会話用の記法を使いません。サーバーは履歴に元の本文を保存し、同じスレッドを Android などから見ると部品を読み取れるテキストに置き換えて配信します。デスクトップの発言全体をコピーする操作でも、部品は読み取れるテキストになります。Markdown を全選択してコピーする場合は元の記述を保持します。

## 同じツリーの JSON 表現

XML と JSON は Rust の同じ部品ツリーに変換されます。JSON は一つのオブジェクトで、その `type` が部品を表します。チェックリストの項目や手順は、`items` や `steps` の配列に入ります。省略時の既定値は [JSON Schema](ui-kit.schema.json) に記載しています。XML の `percent` は `max: 100` の進捗に、`accordion` は `details` に、文章中のキー名やバッジは `text.spans` の要素になります。

````json
{
  "type": "card",
  "title": "I-65 Step 1の証明状況",
  "body": [
    {
      "type": "grid",
      "columns": 2,
      "items": [
        {"type": "stat", "label": "部分証明あり", "value": "10", "unit": "件", "tone": "warn"},
        {"type": "stat", "label": "未証明", "value": "3", "unit": "件"}
      ]
    },
    {"type": "progress", "value": 10, "max": 13, "tone": "warn", "caption": "13件のうち10件に部分証明あり（77%）"}
  ]
}
````

将来の `ui_render` MCP ツールは、この形の JSON をスキーマとモデルの制約で検証する想定です。不正な XML は会話上で元の記述に戻り、エージェントにエラーが届きません。ツール呼び出しならエラーをすぐ返せるため、エージェントがその場で入力を直せます。また、ツール由来の部品をタイムラインイベントとして保存し、既存の部品を ID で参照して進捗などをその場で更新でき、Markdown の自動リンクと競合しません。

## 入力の上限

XML のタグ名と属性名は小文字で、属性値は一重または二重引用符で囲みます。`&amp;`、`&lt;`、`&gt;`、`&quot;`、`&apos;` を使えます。一つの XML ブロックは 16 KiB、JSON の入力は 128 KiB までです。

共通の上限は部品 8 階層・合計 256 個、各子配列（本文、項目、手順、区間、インライン要素）24 件、文字列の合計 16 KiB です。テキスト部品はインラインの要素を合わせて 2,048 UTF-8 バイトまでで、タイトルなどの属性相当の文字列、キー名・コード・バッジ、表のセルは各 512 バイトまでです。表は 24 行・6 列までです。JSON Schema に加え、モデルの検証で UTF-8 のバイト数、階層、セル数の一致、進捗の合計などを確かめます。

部品のテキストを HTML や Markdown として再解釈することはありません。`href` や `src` などの属性、リンク、画像、スクリプト、外部実体、ネットワークアクセスは語彙に含まれません。未知の版・タグ・属性、上限を超えた入力、不正な入れ子や未完成のブロックは元の記述を表示します。JSON を読む関数はエラーを返します。

## 検証

````sh
scripts/shot.sh examples/lab tests/shots/xml-components.json
scripts/shot.sh examples/lab tests/shots/ui-kit.json
scripts/shot.sh examples/lab tests/shots/reply-flowchart.json
````

新しい例の PNG は既定で `$TMPDIR/lab-shots/ui-kit/ui-kit-dark.png` と `ui-kit-light.png` に出力されます（`TMPDIR` を設定していない場合は `/tmp`）。[ヘッドレス撮影](headless.md)に依存関係とソケットの条件を記載しています。

````sh
cargo test --locked -p fuzukue-view -p fuzukue-proto -p fuzukue-i18n
cargo test --locked -p secretaire reply_components
cargo test --locked --manifest-path crates/fuzukue/Cargo.toml --features headless-shot xml_components_test
(cd android && ./gradlew :app:testDebugUnitTest --tests dev.labgui.remote.MarkdownComponentsTest)
````
