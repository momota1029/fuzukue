# Git リポジトリを監視する

記録先にしたいスレッドで「`~/projects/compiler` の `dots-main` を監視して」と受付に頼む。
`watch` の `create` に `name`、ホーム配下の Git リポジトリの `repo`、`kind = "dots"` を渡すと始まる。同じ名前の監視はワークスペースに一つ。
既定のリモートは `origin`、監視間隔は 60 秒（最短 15 秒）。初回は現状を記録し、次の点検から変化を通知する。

`dots` は一つの dot に次の三つの監視を設定する。別の用途では `sources` を指定できる。

- `branch-heads` は `pattern` に一致するブランチの作成と先端の更新を記録する。プリセットは `work/<name>` を対象にする。
- `new-file` は `branch` 上の新しい報告を番号順（`order = "number"`）に記録する。`glob` の一つの `*`、または `regex` の一つの捕捉グループが番号に対応する。プリセットは `notes/handoff/dots-cycle-*.md`、`dots-audit` は `notes/audit/cycle-*.md`。
- `heartbeat` は `branch` 上の `file` を読み、途絶と復帰を記録する。プリセットは `notes/heartbeat/<name>.md`。既定の `stale_minutes` は 25 分で、`status: completed` は完了扱い。

報告のチェックポイントが取得した先端の履歴につながらない場合は、到達できる履歴から前回の報告番号より新しい報告を拾い、以前のチェックポイントと新しい先端を履歴の欠落として記録する。起動直後のリモート取得は最大 3 回試し、取得に失敗した場合は報告のチェックポイントを維持する。

イベントは開始したスレッドに記録する。アーカイブまたは削除した場合、次のイベントで`監視: <name>`を作り、前のスレッドへの参照を一行添える。

`wake = "attention"`（既定）は途絶・復帰・報告履歴の欠落・3 回連続したエラー・判断依頼のある報告で受付を呼ぶ。ブランチの更新は記録だけ残る。
`"all"` は全イベント、`"silent"` は記録のみ。受付への連絡は監視ごとに 10 分に一度までにまとめる。
判断依頼の目印は `attention_patterns` に大文字・小文字を区別しない文字列として指定できる。既定は見出し・箇条書き・太字から始まる行だけを調べる。`attention_marked_lines = false` で通常の行も対象にできる。検索範囲は報告の冒頭 4 MiB。
最近の dots 報告には判断依頼の定型の目印がなく、通常は途絶・復帰・エラーで受付を呼ぶ。判断を求める報告には `## 決定待ち` の見出しを付ける。

`watch` の `list`・`status` で一覧・前回の点検時刻・エラーを表示する。`pause`・`resume` で一時停止・再開、`delete` で解除する。
`test` は一度点検し、検出予定のイベントを返す。監視状態やスレッドへの記録は行わない。監視対象のディレクトリが消えた場合は自動で一時停止する。
設定ファイルに置く場合はプロジェクトの `fuzukue.toml` に追記する。設定の読み込みはワークスペース起動時に行う。

```toml
[[watch]]
name = "dots-main"
repo = "~/projects/compiler"
kind = "dots"
```

静的な監視の追加と削除は TOML で行う。`thread` で記録先を指定でき、省略時は最初のイベントでスレッドを作る。
設定と検出済みの状態はワークスペース状態ディレクトリの `watch.json` に保存する。既定は `~/.local/state/fuzukue/<プロジェクト>/`（追加のワークスペースはその配下の `workspaces/<ID>/`）。静的な設定の定義は TOML に残る。

点検にモデルは使わない。Git は取得したオブジェクトと `refs/fuzukue/watch/` 配下の専用参照だけを更新する。作業ツリー、`origin/*`、`FETCH_HEAD` はそのまま。
サーバーを[サービスとして常駐](service.md)させるとクライアントを閉じても点検が続く。サーバー再起動時は保存済みの状態から再開する。

## 作業プロトコル v1

受付と作業側の間で、専用の Git リポジトリを通じて依頼や報告を受け渡す。
利用するワークスペースごとに、サーバーの状態ディレクトリへ `work-protocol-v1.json` を置く。
設定が空またはファイルがない場合は点検を開始しない。設定の変更はサーバーを再起動して反映する。

```json
[
  {
    "name": "exchange",
    "route": {
      "communication": "team-repository",
      "destination": "/absolute/path/to/communication.git",
      "scope": "project",
      "run": "run-1",
      "private": true
    },
    "writer": "desk",
    "thread": "thread-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
  }
]
```

両側で `route` をそろえ、作業側には `"writer": "worker"` と作業側の記録先スレッドを指定する。
既定の Git オブジェクト形式は SHA-1。通信リポジトリが SHA-256 の場合は両側の設定に `"object_format": "sha256"` を加える。
`name` はワークスペース内の保存名として使う。接続先を変更するときは新しい `name` と `communication` を使う。
`private` は通信先の公開範囲に合わせる。

サーバーは既存の通知を再処理してから設定済みの経路を点検する。受信したイベントは指定スレッドのシステム記録になる。
取得したオブジェクトと送受信の記録は、状態ディレクトリの `work-protocol-v1/<name>/` に保存する。
記録の受け付けに失敗したイベントは保存したまま再試行する。

受付の `watch` ツールで `protocol_routes` を呼ぶと、そのスレッドの経路が分かる。
送信には `protocol_publish` と `route`（上の `name`）、操作ごとに固定の `operation_id`、`event`（`type` と `payload`）を渡す。
受付が提示する接続先・参照・新旧のコミット・イベントの内容を確認し、承認すると送信する。
失敗や拒否で送信待ちのイベントが残った場合、同じ `operation_id` と `event` で再試行できる。
`operation_id` と `event` を両方省略すると、その時点で送信待ちのイベントと受領確認が承認の対象になる。
