# 会話の Markdown に XML 部品を埋め込む

返信本文に、`~~~fuzukue-v1` と `~~~` で囲んだ XML を置くと、対応するデスクトップクライアントが部品として表示します。フェンスは行頭から書き、部品の前後は空行で区切ります。フェンス一つにつき最上位のタグは一つです。閉じる `~~~` の行にも改行を付けます。返信の末尾で改行が削られる場合は、フェンスの後に空行と説明文を続けてください。

MCP ツールの結果をもとに部品を書くときも、返信本文に同じ記法を使います。ツール結果自体には手を加えず、本文でデッキと指摘 ID を参照します。要点がタグ内にしか現れないと、未対応クライアントではコードを読まないと伝わりません。結論などの重要事項は周りの Markdown にも書きます。

## コールアウト

`severity` は `info`、`warn`、`error` のいずれかです。`title` は空でない一行のタイトルです。本文はプレーンテキストとして表示されます。

````markdown
証明の確認で一点残っています。

~~~fuzukue-v1
<callout severity="warn" title="仮定の確認">
定数が因子の個数に依存しないことを確かめます。
</callout>
~~~

比較に使った条件は本文を参照してください。
````

## 表

`table` に `row` と `cell` を置きます。最初の行が見出しです。各行のセル数を揃えます。セルの内容はプレーンテキストです。

````markdown
~~~fuzukue-v1
<table>
  <row><cell>確認項目</cell><cell>状態</cell></row>
  <row><cell>定理の仮定</cell><cell>確認済み</cell></row>
  <row><cell>一様な評価</cell><cell>要確認</cell></row>
</table>
~~~

一様な評価を次に確認します。
````

## 指摘台帳への参照

`deck` はプロジェクト相対のデッキパス（ルートなら `.`）、`id` は指摘台帳にある小文字の UUID です。たとえば、MCP の `list_review_findings` で得た実在する ID を指定します。表示中のデッキの読み込み済みスナップショットから、要約・重要度・状態を表示します。デッキが開いていない場合や指摘が見つからない場合も、参照先のデッキと ID を表示します。

````markdown
~~~fuzukue-v1
<finding deck="research/concentration" id="cb38ac8b-d670-47be-91e3-9c4ecdb61d26"/>
~~~

台帳の指摘では一様性の明記を求めています。
````

## 記法と互換性

生の XML タグを Markdown に置くと、既存の HTML 解釈と重なり、未対応の表示でタグが消えることがあります。フェンスを使うと、同じ本文を既存の Markdown レンダラーでコードとして読めます。サーバーはストリームや履歴を接続ごとに変換せず、そのまま配信します。対応クライアントは、`Hello.markdown_components` にタグと対応するスキーマ版の組（例 `{"tag":"callout","version":1}`）を載せます。Android はこの項目を省略します。項目がない場合は対応タグが空です。

タグ名と属性名は小文字で、属性値は一重または二重引用符で囲みます。`&amp;`、`&lt;`、`&gt;`、`&quot;`、`&apos;` を使えます。部品内で Markdown、HTML、URL、スクリプト、外部実体を解釈したり、ネットワークへアクセスしたりする機能はありません。XML は 16 KiB、属性とセルは各 512 バイト、コールアウト本文は 2,048 バイト、表は 24 行・6 列までです。未知の版・タグ・属性、範囲外の値、不正な入れ子や未完成のフェンスはコードのまま表示されます。スキーマの意味を変えるときは、対応するタグの版とフェンスの版を追加します。

受信中は閉じフェンスの行末までコードとして表示します。見出し、本文、表の行とセルは出現順に読み、コールアウトの種類も文字で示します。部品は表示専用で、Tab の移動先を追加しません。将来操作を追加する場合は、対応する部品とツールの対応をクライアント側で固定し、ボタンを押したときに既存のツール承認経路へ要求を送ります。

## 検証

```sh
scripts/shot.sh examples/lab tests/shots/xml-components.json
```

出力は既定で `${TMPDIR:-/tmp}/lab-shots/xml-components/xml-components.png` です。[ヘッドレス撮影](headless.md)に必要な依存関係とソケットの条件を記載しています。

```sh
cargo test --locked -p fuzukue-view -p fuzukue-proto -p fuzukue-i18n
cargo test --locked --manifest-path crates/fuzukue/Cargo.toml --features headless-shot xml_components_render_only_after_the_stream_closes_the_fence
(cd android && ./gradlew :app:testDebugUnitTest --tests dev.labgui.remote.MarkdownComponentsTest)
```
