# 拡張機能

拡張機能は、規則パックと同じディレクトリ構成に `extension.toml` と任意の WASM コンポーネントを加えたものだ。第1段のクライアントはプロトコル27以上で、拡張機能の設定と画面を表示する。

## インストールと更新

プロジェクトで共有するときは `fuzukue.toml` に書く。個人用はサーバーを起動する利用者の `$XDG_CONFIG_HOME/fuzukue/extensions.toml`（既定は `~/.config/fuzukue/extensions.toml`）に同じ形式で書く。

```toml
[[extensions]]
source = "github:owner/repo"
rev = "v0.1.0"

[[extensions]]
source = "../extensions/memo-stats"
```

ローカルの相対パスは設定ファイルのディレクトリから解決し、絶対パスも使える。Git の場合はタグ、ブランチ、コミットを `rev` に指定する。取得にはサーバーの Git の認証設定を使い、規則パックのキャッシュを共有する。プロジェクトまたは個人設定のディレクトリにある `fuzukue.lock` の `[[extensions]]` に、Git のコミットと、コンポーネントがある場合はその SHA-256 を記録する。

設定ファイルに項目を追加・削除したらサーバーを起動し直す。設定の「拡張機能」で権限を確認し、許可すると有効になる。「確認して更新」で指定した取得元を再取得し、固定先と変更ファイルの一覧を更新する。ローカルパッケージをビルドし直したときもこの操作で SHA-256 を更新する。Git の元の版がキャッシュから消えていた場合は変更ファイルの比較を省く。

許可と無効化の設定はワークスペースの利用者状態に保存する。取得元と版ごとに許可を記録し、次の版が同じ権限かその一部だけを求めるときは許可を引き継ぐ。パスの glob は文字列単位で比較する。新しい権限や対象の追加、同じ版名でのコンポーネントやマニフェストの差し替えには承認が要る。無効化すると道具、画面、イベント購読、宣言ファイルの適用が止まる。

## マニフェストと権限

```toml
name = "memo-stats"
version = "0.1.0"
title = "title"
description = "description"
author = "fuzukue"
component = "extension.wasm"
view = true
events = ["card-saved", "memo-added"]

[permissions]
read = ["memos/*/card.md"]
# write = []                           # 自分のデータ置き場だけ
# write = ["memos/derived/**/*.md"]     # 明示した書き込み先
# network = ["example.org"]            # HTTP(S) GET
agent-tools = true
# log = true
```

- `read` と `write` はプロジェクト相対の glob を指定する。読み出しはゲストの指定と許可された glob の共通範囲に限る。`write = []` の既定範囲は `.fuzukue/extensions/<name>/` の下。権限を省くとそのホスト関数はリンクされない。パスの `..`、絶対パス、シンボリックリンクを経由する読み書きは拒否する。
- `network` には DNS ホスト名か標準表記の IPv4 アドレスを完全一致で列挙する。HTTP(S) の GET だけを受け付け、リダイレクトと環境変数のプロキシは使わない。
- `agent-tools` がある場合だけ、`<name>.<tool>` を MCP に出す。道具の説明と結果には取得元を付け、結果は信頼されていない外部データとして渡す。
- `log` はメッセージをサーバーのログへ書く。ログは呼び出しごとに件数とサイズを制限する。

承認済みで有効なパッケージの `agents/`、`skills/`、`reviewers/`、`style/`、`lint/`、`policy/`、`templates/`、`locales/` は宣言部分を構成する。規則の優先順位は組み込み、規則パック、個人用拡張、プロジェクトの拡張、プロジェクトの手元の規則の順で、後が優先される。拡張機能の画面の文言は、各パッケージの Fluent カタログで解決する。テンプレートはデッキから `templates/<ファイル>.tex` で参照できる。宣言部分にはエージェントへの指示や文書のテンプレートも含まれるため、承認するときはパッケージ全体を確認する。

## Rust で書く

WIT は [extension.wit](../crates/fuzukue-extension-api/wit/extension.wit) の `fuzukue:extension@0.1.0` の `extension` world を使う。Rust では `fuzukue-extension-api` を依存に加え、`Guest` を実装して `export!` で公開する。別言語でも同じ WIT のコンポーネントを出力すればよい。呼び出しごとに新しいインスタンスを使う。状態を引き継ぐ必要がある場合は許可されたデータ置き場に保存する。

ゲストコードには Wasmtime の epoch による約1秒の実行上限と、線形メモリ1個当たり32 MiB、最大2個の上限がある。ファイルと HTTP 応答にもサイズ制限がある。WASI にはファイル、環境変数、ソケットを渡さない。トラップや時間超過が起きると、サーバーは設定画面とログにエラーを出し、そのサーバーセッションでは拡張を停止する。ゲスト関数が WIT の `result` で返したエラーは、その呼び出しの結果として扱う。

`render-view` と `on-ui-event` は部品の配列を JSON で返す。画面の文字列は `{"key":"title"}` の形で `locales/ja/*.ftl` または `locales/en/*.ftl` を参照できる。件数や計算結果は文字列で直接渡せる。

| `type` | 主なフィールド |
| --- | --- |
| `heading`、`markdown` | `text` |
| `table` | `columns`、`rows`（セル配列の配列） |
| `list` | `items` |
| `input` | `id`、`label`、初期値 `value` |
| `button` | `id`、`text` |
| `tabs` | `id`、`tabs`（`title` と `children` を持つ配列） |

入力欄は Enter で値を送る。ボタンは押すと部品 ID を送る。タブの切り替えはクライアントで行う。Markdown はテキスト表示に使い、リンクと画像は無効になる。 `on-event` には保存したカードか追加したメモのプロジェクト相対パスを渡す。 `list-tools` に登録する説明も Fluent のキーを使える。 `invoke-tool` の引数は JSON で渡り、結果はテキストで返す。

## 見本をビルドする

[`memo-stats`](../examples/extensions/memo-stats/) は、メモの件数と、本文を空白で区切った語数を集計する。完成したコンポーネントも置いてある。ソースを変更した場合は、リポジトリ直下で以下を実行する。

```sh
rustup target add wasm32-wasip2
cargo build --offline --release --target wasm32-wasip2 -p fuzukue-memo-stats-example
cp target/wasm32-wasip2/release/fuzukue_memo_stats_example.wasm examples/extensions/memo-stats/extension.wasm
```

`examples/extensions` をプロジェクトとして開ける。初回の権限を許可すると、左の欄に「メモの統計」が出る。画面と `memo-stats.memo_stats` の道具は、同じプロジェクトのメモを読む。

コマンド実行、スレッド操作、資料庫への新種別の追加、配布索引、署名は次の段で扱う。
