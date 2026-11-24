# リリースマニフェスト

スキーマ 1 は UTF-8 の JSON。最上位の項目は `schema_version`、`channel`（`stable`／`beta`）、`version`（SemVer）、`commit`（完全な Git SHA）、`commit_date`（コミッター日時）、`created_at`（生成日時）、`protocol`、`min_client_protocol`、`min_server_protocol`、`artifacts`、`next_public_keys`。日時は RFC 3339。成果物の項目は `platform`、`name`、`size`（バイト数）、`sha256`（小文字の16進数）、`url`（HTTPS）、`kind`（`server`／`windows-client`／`linux-client`／`android-apk`）、`changed_since_previous`。

署名対象は、各オブジェクトを上記の項目順に並べ、成果物を `name` 順、次の公開鍵を小文字16進数の文字列順に並べた JSON。日時は UTC の `Z` 表記にそろえ、`serde_json` のコンパクト形式で出力する。末尾の改行も付けない。未知の項目、重複した項目、重複した成果物名は拒否する。

## 手元の成果物から作る

ビルド対象のコミットと成果物を指定する。`path` は設定ファイルからの相対パスまたは絶対パス。ファイル名が成果物の `name` になる。`commit` は省略すると `HEAD`、`created_at` は省略すると生成時刻になる。次の公開鍵を告知するときは、`next_public_keys` に公開鍵を追加する。

```json
{
  "channel": "beta",
  "version": "1.2.0-beta.1",
  "protocol": 48,
  "min_client_protocol": 40,
  "min_server_protocol": 42,
  "artifacts": [
    {
      "kind": "server",
      "platform": "linux-x86_64",
      "path": "dist/secretaire-linux-x86_64",
      "url": "https://example.invalid/releases/v1.2.0-beta.1/secretaire-linux-x86_64"
    }
  ]
}
```

```sh
secretaire release manifest-from-build build.json --repo . --previous previous.json > release.json
```

初回は `--previous` を省く。以後は信頼できる同一チャネルの前回マニフェストを指定する。前回と同じ種類・プラットフォームの成果物はサイズと SHA-256 で変更を判定する。前回のコミットとの差分を Git で調べ、プロトコルや該当クライアントの入力に変更がなければ、省略したクライアント成果物の URL とハッシュを引き継ぐ。変更があれば該当する成果物を指定する。Android にはリリース添付 APK の URL を使う。

公開工程の TODO は `secretaire release sign release.json --signing-key <file> > release.sig` に相当する署名ツールの用意。署名ファイルは `key_id`（32 バイトの生公開鍵の SHA-256）と `signature`（64 バイトの Ed25519 署名）を持つ別の JSON で、値は小文字の16進数。

## 検証と選定

```sh
secretaire release verify release.json release.sig --pubkey pinned.pub --artifact-dir dist
```

固定公開鍵のファイルは 32 バイトの公開鍵を小文字の16進数で記した1行。`--pubkey` を繰り返して複数の鍵を固定できる。`--artifact-dir` を指定すると、引き継いだ分を含め、記載された全ファイルのサイズと SHA-256 を検証する。次の鍵は、固定済みの鍵で署名を検証し、版を受理した後に信頼リストへ追加できる。CLI は検証だけを行う。

選定ライブラリは指定チャネルの検証済みマニフェストから、双方の対応プロトコル範囲が重なる最新版を選ぶ。`protocol` は上限で、接続相手の役割に応じて `min_client_protocol` または `min_server_protocol` を下限に使う。受理済みの版とコミッター日時の最大値をチャネルごとの基準として、どちらかが古ければ明示的な強制指定がある場合に限って許可する。強制指定でも基準は下げず、同じ版番号で内容が異なるマニフェストは拒否する。
