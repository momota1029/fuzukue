# Linux で Workbench の画面を確認する

`scripts/shot.sh` は Linux 上に専用の仮想 X11 ディスプレイ（Xvfb）を起動します。実際の GPUI Workbench を描画し、WebSocket で secretaire に接続して JSON シナリオを再生します。WSL でも Windows のデスクトップにはウィンドウが出ません。

実クライアントの「画面を撮る」は、利用者の作業台を添付・メモ・報告に使う機能です。ヘッドレス撮影は隔離した画面で UI を検証するための機能です（[画面写真の設計](../cards/design/client-screenshots.md)）。

## 実行方法

Debian / Ubuntu では、通常の Rust・C ビルド環境に加えて次のパッケージが必要です。

```sh
sudo apt install xvfb xauth x11-utils imagemagick python3 libvulkan1 mesa-vulkan-drivers \
  libfontconfig1 libxcb1 libxkbcommon0 libxkbcommon-x11-0 \
  fonts-ipaexfont-gothic fonts-ipaexfont-mincho fonts-dejavu-core fonts-dejavu-mono
```

```sh
scripts/shot.sh examples/lab tests/shots/chat.json
scripts/shot.sh examples/lab tests/shots/chat-live.json
scripts/shot.sh examples/lab tests/shots/xml-components.json
scripts/shot.sh examples/lab tests/shots/ui-kit.json
scripts/shot.sh examples/lab tests/shots/reply-flowchart.json
scripts/shot.sh examples/lab tests/shots/tiers.json
scripts/shot.sh examples/lab tests/shots/trash.json
scripts/shot.sh examples/lab tests/shots/rewrite.json
scripts/shot.sh examples/lab tests/shots/composer.json
scripts/shot.sh examples/lab tests/shots/sidebar.json
scripts/shot.sh examples/lab tests/shots/file-browser-tree.json
scripts/shot.sh examples/lab tests/shots/distribution.json
scripts/shot.sh examples/lab tests/shots/self-update.json
scripts/shot.sh examples/lab tests/shots/library.json
scripts/shot.sh examples/extensions tests/shots/extensions.json
scripts/shot.sh examples/lab tests/shots/memos.json
scripts/shot.sh examples/lab tests/shots/review-reports.json
scripts/shot.sh examples/lab tests/shots/ledger.json
scripts/shot.sh examples/lab tests/shots/paper.json
scripts/shot.sh examples/lab tests/shots/open-paper.json
scripts/shot.sh examples/lab tests/shots/card-edit.json
scripts/shot.sh examples/lab tests/shots/change-history.json
scripts/shot.sh tests/shots/fixtures/editor tests/shots/editor.json /tmp/lab-shots/editor
scripts/shot.sh tests/shots/fixtures/lsp tests/shots/lsp.json /tmp/lab-shots/lsp
scripts/shot.sh examples/lab tests/shots/symbols.json /tmp/lab-shots/symbols
scripts/shot.sh examples/lab tests/shots/math-select.json
scripts/shot.sh examples/lab tests/shots/pdf-popup.json
scripts/shot.sh examples/lab tests/shots/comment-side-thread.json
scripts/shot.sh examples/lab tests/shots/thread-info.json
scripts/shot.sh examples/lab tests/shots/folder-grants.json
scripts/shot.sh examples/lab tests/shots/right-pane-resize.json
scripts/shot.sh tests/shots/fixtures/card-markdown tests/shots/card-markdown.json
scripts/shot.sh examples/lab tests/shots/chat-scroll-growth.json
scripts/shot.sh --git examples/lab tests/shots/git.json
scripts/shot.sh examples/lab tests/shots/thread-menu.json
scripts/shot.sh examples/lab tests/shots/codex-partner.json
scripts/shot.sh examples/lab tests/shots/roles.json
scripts/shot.sh examples/lab tests/shots/routing.json
scripts/shot.sh examples/lab tests/shots/review-agents.json
```

第 3 引数を省略すると、`${TMPDIR:-/tmp}/lab-shots/<シナリオ名>/` を使います。コピーやリンク用ファイルを置く一時ディレクトリも `${TMPDIR:-/tmp}` に作ります。`TMPDIR` に指定するディレクトリは、撮影用にコピーするプロジェクトの外に用意してください。成功した PNG のパスを標準出力に、ビルドやエラーの情報を標準エラー出力に出します。UI 変更の報告には、この PNG パスを添付してください。

ローカル実行では、プロジェクトを一時ディレクトリにコピーします。Git メタデータ、ビルドキャッシュ（`.lake`、`target`）、コピーの外を指すシンボリックリンクは除きます。サーバーは隔離した設定・状態ディレクトリと動的に割り当てられたループバックポートを使い、終了時に停止します。固定応答で再現する Claude Code（`tests/shots/claude`）と Codex app-server（`tests/shots/codex`）を使い、分離した PATH で起動します。シナリオに `build_paper` がある場合は、その PATH に PDF ビルド用の CLI も追加します。自動査読はシナリオに `"auto_review": true` がある場合だけ有効です。自動ビルドは無効、自動承認は手動確認に設定しています。チャットシナリオの「承認」付きの発言では、ツールの表示と承認リクエストまで再現します。

Linux では起動前に、フィクスチャ用の `127.0.0.1` の TCP リスナーと Xvfb 用の Unix ソケットを検査します。その操作が権限で拒否された場合は、`headless capture needs loopback sockets; this environment forbids them` と 1 行出力して終了コード `77` で撮影をスキップします。起動済みサーバーを使う `--url` では Xvfb 用の Unix ソケットを検査します。

作業役にはジョブ専用の `TMPDIR` が渡ります。Codex の Linux プロファイルでネットワークを無効にすると、ループバック TCP も Unix ソケットの `bind` も拒否されます。`network.enabled = true` はネットワークを有効にする設定です。プロキシの宛先設定から、撮影に必要な Unix ソケットの `bind` だけをジョブ領域に許可することはできません。Claude Code の起動時に設定するファイル用フックも、外側のサンドボックスのソケット制限は変更できません。撮影をスキップした場合は、撮影に必要なソケットを使える環境で実行してください。[ワークスペースの設定](../cards/design/workspaces.md) [Codex の Linux ソケット制限](https://github.com/openai/codex/blob/rust-v0.169.0/codex-rs/linux-sandbox/src/seccomp.rs)

起動済みサーバーの表示を確認する場合は `--url` を使えます。

```sh
scripts/shot.sh --url ws://127.0.0.1:7878/ws /tmp/read-only.json /tmp/lab-shots/live
```

このモードでは WebSocket の初期ハンドシェイクでスナップショットとイベントを受け取り、必要に応じて表示用ファイルを HTTP GET します。サーバーの変更を伴う送信操作は遮断されます。デッキの選択を変更する送信操作も遮断されるため、`open_paper` ステップで開けるのはサーバーの共有デッキです。認証が必要な `ws://` のサーバーでは `FUZUKUE_SHOT_TOKEN` を環境に設定できます。トークン付き接続には、信頼できるループバックまたはトンネルを使ってください。

`composer_status_fits_and_aligns_with_stop` は、600 px と 1440 px のウィンドウ幅で、入力欄の使用量リングと停止ボタンの中心が水平方向に 1 px 以内で揃い、状態欄の操作が一行に収まることを確認します。ライトとダークの両方で、リングのツールチップが入力欄の上に収まることも確認します。

## 作業役からの撮影

`scripts/shot.sh` が終了コード `77` で止まったら、`list_screenshot_scenarios` でフィクスチャと画像名を確認し、`capture_screenshot` を呼びます。たとえば `{"scenario":"sidebar","names":["sidebar-dark"]}` を渡すと、サーバーが共有作業ツリーの未コミット変更を含めてビルドし、隔離した画面で撮影します。撮影は受付からも呼び出せます。サーバーがビルドする作業ツリーには信頼できるコードを置いてください。

結果に表示される画像で UI を確認します。画像を表示できない場合は、結果の PNG パスを開きます。出力は `target/shots/<ジョブ ID またはセッション ID>/<時刻>/` に置きます。作業役からそこを開けない場合はジョブの一時ディレクトリに置きます。各保存先には直近 10 回分を残します。`names` を省略すると全画像を撮り、結果には最大 3 枚を添付します。対応するシナリオでは `theme`（`light` / `dark`）と `size`（`width` / `height`）を上書きできます。

撮影はサーバー全体で順番に実行されます。撮影開始からの上限は既定で 20 分で、`server.toml` の `[screenshots].timeout_seconds` で変更できます。MCP 用シナリオの JSON に `"mode": "url"` を設定すると、`[screenshots].url` に設定した検証用サーバーのループバック URL（例 `ws://127.0.0.1:7879/ws`）へ読み取り専用で接続します。

## シナリオを書く

シナリオは `steps` 配列を持つ JSON です。配列の上から順に実行します。

```json
{
  "steps": [
    {"action": "size", "width": 1120, "height": 900},
    {"action": "theme", "theme": "light"},
    {"action": "connect"},
    {"action": "open_paper", "deck": "papers/levy-families"},
    {"action": "scroll", "pane": "paper", "pixels": 400},
    {"action": "screenshot", "name": "paper"}
  ]
}
```

| `action` | 指定するキーと動作 |
| --- | --- |
| `size` | `width` と `height`。既定は 1440×900。320×240 から 3840×2160 の範囲で変更できます。 |
| `theme` | `theme` に `light` または `dark`。 |
| `locale` | `locale` に `ja` や `en`。開いているウィンドウに適用します。最初の接続時からこの言語を使う場合は `connect` より前に指定します。 |
| `comment_destination` | `side_thread` が `true` なら論文のコメントを別スレッドで反映し、`false` なら今のスレッドに送る（既定）。 |
| `connect` | シナリオで一度だけ実行し、初期のワークスペース、論文、スレッド一覧の受信を待ちます。 |
| `distribution_preview` | サーバー設定に撮影用の接続 URL と更新通知を表示します。 |
| `self_update_preview` | サーバー設定に撮影用のコミット、進捗、工程ログを表示します。 |
| `wait_event` | `event` にプロトコルの `type`（例 `approval_requested`）、任意で `timeout_ms`。到着済みのイベントも数え、同じ種別で繰り返すと次の 1 件を待ちます。 |
| `resolve_approval` | 選択中のスレッドで最初の承認待ちのボタンをクリックします。`decision` は `accept` または `decline`。ローカルのフィクスチャ専用です。 |
| `wait_git` | Git の状態が届くまで待ちます。任意の `path` は変更一覧の相対パス、`diff` は開いた差分に含まれる文字列、`commit` は変更がなくなった時点の最新コミットの件名です。 |
| `wait_idle` | 選択中のスレッドが実行中でなく、受信のない状態が `quiet_ms` 続くまで待ちます。既定は 200 ms。任意で `timeout_ms`。承認待ちには `wait_event` を使います。 |
| `open_workspace` | フィクスチャの `path` を登録して開き、パス末尾が `path_tail` のワークスペースのスレッド一覧を待ちます。 |
| `switch_workspace` | パス末尾が `path_tail` のサイドバー行をクリックし、切り替え先のスレッド一覧を待ちます。フィクスチャ専用です。 |
| `select_thread` | `title` に一覧で一意の完全なスレッド名。 |
| `seed_thread_switch` | 300 発言ずつの履歴を持つスレッドを 2 件、クライアント内に作って最初のスレッドを選びます。 |
| `assert_active_thread` | 選択中のスレッド名が `title` と一致することを確認します。 |
| `open_thread_menu` | 選択中のスレッド行の「…」をクリックします。 |
| `hover_thread` | `title` で指定したスレッド行にポインターを置きます。 |
| `wait_thread_pinned` | `title` で指定したスレッドのピン留めが一覧に反映されるまで待ちます。 |
| `wait_thread_archived` | `title` と一致するスレッドのアーカイブが一覧に反映されるまで待ちます。 |
| `assert_delegate_report` | `model` の作業者の完了報告が届くまで待ちます。 |
| `wait_delegate_text` | 実行中の作業役の発言に `text` が含まれるまで待ちます。 |
| `wait_partner_text` | 相談役の発言に `text` が含まれるまで待ちます。 |
| `wait_responding` | 入力欄に停止ボタンが表示され、入力欄の上に `status` と一致する応答状態が表示されるまで待ちます。 |
| `wait_reviews` | `deck` と `kinds` で指定した査読がすべて実行中（`running: true`）か完了済み（`running: false`）になるまで待ちます。 |
| `assert_model` | 選択中のスレッドの相談役のモデル ID が `model` と一致することを確認します。 |
| `assert_worker_model` | 選択中のスレッドの作業役のモデル ID が `model` になるまで待ちます。 |
| `upload` | `name` の一時ファイルに `text` を書き、選択中のスレッドの通常の添付処理でアップロードします。ローカルのフィクスチャ専用です。 |
| `wait_inbox_proposal` | 選択中のスレッドに `to` の保存先提案が届くまで待ちます。 |
| `assert_order` | 要素 ID の `above` が `below` より上に描画されたことを確認します。 |
| `assert_thread_info_layout` | 会話ヘッダーの上に全幅の行がないことを確認します。`open` が `true` の場合は、スレッド情報欄の内側で「テスト」と閉じるボタンが横に並ぶことも確認します。`false` の場合は、情報欄と閉じるボタンが消えたことを確認します。 |
| `wait_element` | `id` の要素が描画されるまで待ちます。 |
| `wait_trash_title` | `title` と一致する項目のタイトルがゴミ箱に描画されるまで待ちます。 |
| `start_comment` | ローカルのフィクスチャで、`card` のコメント入力欄を開きます。任意の `quote` で引用を指定できます。 |
| `new_thread` | 新しいスレッドを作り、選ばれるまで待ちます。任意の `deck` で添付先のプロジェクト相対パスを指定できます。 |
| `attach_deck` | `deck` を選択中のスレッドに添付し、一覧への反映を待ちます。ローカルのフィクスチャ専用です。 |
| `open_paper` | `deck` にプロジェクト相対のデッキディレクトリ。論文編集を開き、本文を待ちます。任意の `via` に要素 ID を指定すると、該当要素をクリックして開きます。 |
| `build_paper` | 開いている論文の PDF を手動ビルドし、結果を待ちます。ローカルのフィクスチャ専用です。 |
| `wait_pdf` | 論文の PDF 表示で先頭ページの描画を待ちます。 |
| `click_pdf_card` | SyncTeX の対応情報を使い、`card` の矩形を画面に出して実際にクリックします。指定カードのポップアップが開くまで待ちます。 |
| `scroll` | `pane` に `paper` または `chat`、`pixels` に移動量。正で下、負で上です。 |
| `wheel` | `pane`（`paper` / `chat`）の中央に本物の ScrollEvent を送ります。`pixels`・`pages`・`lines`（既定 -3）のどれかを指定します。 |
| `log_scroll` | 論文とチャットのスクロール位置を標準エラーに出します。 |
| `assert_card_scrollbar_drag` | 開いている長い Markdown カードのつまみをドラッグし、つまみの位置・長さ、本文の高さ、スクロール範囲をフレームごとに検証します。 |
| `assert_transcript_scroll_growth` | 未接続の会話にフィクスチャを作り、前後に行が増えたときの表示位置、ドラッグ中のつまみ、末尾追従を検証します。 |
| `scroll_to_card` | `id` の論文カードへ移動します。証明の開閉は `expand_proofs` やクリックで操作できます。 |
| `wait_paper_card` | `deck` の論文が開き、`card` へのスクロールが完了するまで待ちます。任意の `text` を指定すると、エディタが閉じ、表示する本文にその文字列が反映されるまで待ちます。 |
| `expand_proofs` | 開いている論文の証明をすべて展開します。 |
| `click` | 表示されている GPUI の要素 `id`（例 `new-thread`、`chat-input`）、または左上原点の `x` と `y`。ID が重複しない要素を使ってください。 |
| `ctrl_click_text` | 論文の `card`（カード ID）の主張にある `text` を Ctrl+クリックします。 |
| `hover` | 表示されている GPUI の要素 `id` の中央にポインターを移動します。 |
| `drag` | 左上原点の `from_x`、`from_y` から `to_x`、`to_y` まで左ボタンでドラッグします。 |
| `drag_from_element` | 表示されている要素 `id` の中央から、右・下を正とする相対座標 `dx`、`dy` まで左ボタンでドラッグします。数式を指定する場合、要素 ID は `formula:` に前後の空白と区切りを除いた TeX をつなげたものです。 |
| `assert_sidebar_width` | `width` とサイドバーの描画幅、設定の幅がそれぞれ 1 px 未満の差で一致することを確認します。 |
| `assert_file_tree_labels` | `paths` の各フォルダー名に 2.5 rem 以上の表示幅があることを確認します。空文字列はルートを表します。 |
| `type` | フォーカス中の入力欄に `text` を入力します。日本語や改行も指定できます。 |
| `key` | `key` に `Ctrl+Enter`、`shift-tab`、`pagedown` などのキー。 |
| `wait` | `ms` の間待ちます。 |
| `screenshot` | `name` に拡張子なしのファイル名。半角英数字、ハイフン、アンダースコアを使えます。 |

イベント待ちのタイムアウトの既定値は 15 秒です。位置がレイアウトによって変わる操作では、`click` の座標よりも要素 ID を使うと安定します。ID は画面を作る Rust コードの `.id(...)` やボタンのコンストラクターで確認できます。仮想リストの画面外にある要素は、スクロールして表示してからクリックしてください。

`chat.json` は Markdown と論文マクロを使う数式入りのユーザー発言、Markdown 装飾付きの入力欄、Claude Code セッション、ツールグループ、未承認リクエスト、作成欄ツールバーをライトとダークで撮ります。拒否後の折り畳まれた行を `chat-approval-folded.png` に撮ります。`sidebar.json` はライトとダークでピン留め、ホバー時のスレッド操作、接続メニュー、パンくず付きのサーバーフォルダ選択を撮ります。表示メニューから資料庫を開く操作、ヘルプメニューから開く設定、ドラッグによる幅変更も確認します。`paper.json` は添付論文を開く操作、狭い幅での本文、定理枠、別行立て数式、証明と番号付きリストを確認します。

`codex-partner.json` はスレッド情報欄のモデル一覧、Codex の返信、重い作業の最終報告、相手の ID と使用量を撮ります。`chat-live.json` は会話末尾の応答状態、作業役の発言、折りたたまれた道具の件数、最終報告を撮ります。`agents.json` は並行する作業の状態が入力欄の上で一行にまとまる様子と、ホバーで作業ごとの詳細が開く様子を撮ります。撮影モードではアニメーションの位相を固定します。

`tiers.json` は会話中の作業の詳細に段階とモデルが表示されるところを `tier-details.png` に、設定のエージェント欄で段階別の集計が表示されるところを `tier-usage.png` に撮ります。`open_delegate_details` は会話中の最後の作業の詳細をクリックします。

`chat-live.json` の `delegate-below-reply.png` は、受付の返事と作業役の欄の順序を確認してから撮ります。`trash.json` はメモの削除確認とゴミ箱を `trash-confirmation.png`、`trash.png` に撮ります。`rewrite.json` は発言の操作列と作り直した返事を `rewrite-action.png`、`rewrite-result.png` に撮ります。

`roles.json` は Codex の相談役から Claude の作業役に委譲します。作業中に「作業役へ」のまま追加の指示を送り、その発言と送信先を `roles-steering.png` に撮ります。報告を受けた相談役の締めは `roles-complete.png` に撮ります。

`routing.json` は新規スレッドで相談を送り、Claude の応答にあるカード ID で論文に付くと、取り消し付きの通知を `/tmp/lab-shots/routing/routing-auto-attach.png` に撮ります。`sources.bib` を添付して、添付の「…」メニューに出る保存先の提案を `routing-inbox.png` に撮り、提案を選んで確定します。Ctrl+K に「Lévy 族」を入力した結果を `routing-search.png` に撮ります。
`review-agents.json` は導入カードを編集し、約 5 秒後に Claude の文章査読と Codex の数学査読が同時に動くところで追加の質問を送ります。相談役の返信と査読の経過時間を `review-agents-running.png` に、査読完了後の「指摘 1 件」を `review-agents-done.png` に撮ります。最後に文章査読の件数をクリックして、導入カードへ移動することを確認します。
`consistency-review.json` は論文ビューで定義カードを保存し、整合確認の実行中の行を `consistency-review-running.png` に、指摘 1 件の行を `consistency-review-done.png` に撮ります。
`routing.json` は新規スレッドで相談を送り、Claude の応答にあるカード ID で論文に付くと、取り消し付きの通知を `/tmp/lab-shots/routing/routing-auto-attach.png` に撮ります。`sources.bib` を添付して、添付の「…」メニューに出る保存先の提案を `routing-inbox.png` に撮り、提案を選んで確定します。Ctrl+K に「Lévy 族と球面の集中」を入力して、候補に「スライド」が付く様子を `routing-search.png` に撮ります。領域外をクリックして閉じた状態を `routing-search-closed.png` に撮ります。

`extensions.json` は初回の権限ダイアログを `extension-permissions.png`、承認後に表示メニューから開いた「メモの統計」を `extension-memo-stats.png` に撮ります。

`open-paper.json` はチャットから Claude のフィクスチャに MCP の `open_paper` を呼び出させ、添付された論文のカードとチャットを撮ります。`card-edit.json` はカードの編集アイコンをクリックし、編集中と保存後の再描画を撮ります。編集アイコンの ID は本文が `edit-paper-statement-<カード ID>`、別ファイルの証明が `edit-paper-proof-<カード ID>` です。

`change-history.json` は人の保存をカードの履歴から開いて `change-history-card.png` に、Claude の発言の操作列を `change-history-actions.png` に、操作列から開いた差分を `change-history-turn.png` に撮ります。

`symbols.json` は球面のオブザーバブル直径の原稿で「Lévy 族」を Ctrl+クリックし、別デッキの定義カードへ移動します。「使っている所」を `/tmp/lab-shots/symbols/symbols-references.png` に撮ります。

`pdf-popup.json` は論文を手動ビルドし、中央の PDF、球面の集中定理のハイライトとポップアップ、ポップアップ内の TeX エディタを撮ります。`latexmk`、`uplatex`、`dvipdfmx`、`pdflatex`、`pdfinfo`、`kpsewhich` を用意してください。

`math-select.json` は球面の集中定理のインライン数式から後続の日本語へドラッグし、選択と「コメント」ボタンを `math-selection.png`、ボタンから開いた TeX 引用付きの下書きを `math-comment-draft.png` に撮ります。

`git.json` は情報欄の Git、ファイル差分、コミット完了を `git-status.png`、`git-diff.png`、`git-commit.png` に撮ります。`--git` はコピー先を `main` ブランチの Git リポジトリとして初期化し、専用の著者情報で初回コミットを作ってから `git-shot.txt` を変更します。元のプロジェクトには触れません。

`workspaces.json` は 2 つのフィクスチャを切り替え、`workspace-sidebar.png` と `workspace-switched.png` を撮ります。既定のプロジェクトは一時ホームの `~/project` にコピーし、`--workspace` で指定した 2 つ目は `~/workspace-other` にコピーします。

```sh
scripts/shot.sh --workspace examples/code-pilot examples/lab tests/shots/workspaces.json
```

`workspace-requests.json` は同じ二つのフィクスチャと一時端末トークンを使います。通常の会話で依頼を下書きし、別のワークスペースに移動してから送り元の Inbox で確認・送信します。受信側の Inbox からの着手と、質問・回答・結果の往復も通します。出力は `workspace-request-preview.png`、`workspace-request-pending-inbox.png`、`workspace-request-inbox.png`、`workspace-request-question.png`、`workspace-request-result.png` です。

```sh
scripts/shot.sh --workspace examples/code-pilot examples/lab tests/shots/workspace-requests.json
```

## Linux の差と Windows で確認する項目

- 描画は Mesa Lavapipe を使用します。既定パスにない場合は `FUZUKUE_SHOT_VK_DRIVER` で ICD の JSON を指定できます。
- フォント探索は IPAex Gothic / Mincho と DejaVu Sans / Serif / Sans Mono に固定します。検証に使った版は IPAex 004.01、DejaVu 2.37 です。Windows の游ゴシックや游明朝とは字形、フォールバック、改行位置が異なります。同じ Linux 環境で PNG を比較してください。
- PNG は描画領域のサイズと単色画像の有無を自動検査します。本文の右端、改行、数式の配置は PNG で確認してください。
- タッチジェスチャー、IME の変換中表示と候補ウィンドウ、Windows のフォントと DirectX 描画、OS ファイル選択ダイアログは Windows 実機で確認してください。カードの本文と証明のエディタでは、日本語入力中の表示、候補の確定、Ctrl+Enter と Esc の挙動も確認してください。シナリオの `type` は確定した文字列を GPUI に渡す操作です。
- clang + mold で `library not found: xcb` などが出る場合は、実行用の共有ライブラリはあってもリンク用の `.so` 名が未インストールの場合があります。`shot.sh` は一時的なリンク名を補います。シェルから直接クライアントのテストをリンクする環境には `libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev` を入れてください。クライアントの `cargo check` には `RUST_FONTCONFIG_DLOPEN=1` を使います。
- 論文の PDF 表示では、高 DPI やウィンドウのリサイズ後もクリック位置とハイライトが一致すること、PDF の文字選択がコメントの引用に入ること、保存と再ビルドの後に表示位置が保たれることを確認してください。
- 論文カードでは、インライン数式と別行立て数式を起点・終点に前後の本文や段落をまたいで選択し、Ctrl+C とコメントの引用に区切り付きの TeX が入ることを確認してください。ダブルクリックで数式を、トリプルクリックで数式を含む段落を選択できることも確認してください。
- マウス設定を「1 画面ずつスクロール」に変え、PDF のホイール 1 ノッチでウィンドウの高さの約 9 割だけ進むことを確認してください。PDF にフォーカスを置き、PageUp・PageDown、Space・Shift+Space、Home・End と矢印キーでも移動を確認してください。

### 研究 Issue の作業場

```sh
scripts/shot.sh examples/lab tests/shots/workshop.json
```

`wait_research_issue` を含むシナリオは、コピーした研究室に一時 Git の `main` を作り、`tests/shots/gh` と作業役のスタブを利用します。 `--workshop` でもこの準備を明示できます。[作業場の撮影シナリオ](../tests/shots/workshop.json) は、スタブの Issue #12 と Hamming 距離のカードで着手可、作業中、確認待ちを通す。出力は `workshop-list.png`（中央の一覧）、`workshop-ready.png`（本文と履歴）、`workshop-running.png`（新規スレッドと作業役）、`workshop-needs-review.png`（報告と「取り込む」）。GitHub や Lake の実環境は使用しない。

`review-reports.json` はサンプル投稿を使い、論文カードの査読マーカー、投稿の指摘表、状態絞り込みから開いた指摘の詳細を撮ります。PNG は `review-card-marker.png`、`review-points.png`、`review-point.png` です。
