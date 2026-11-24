# 本を講義ノートにする流れ（設計案）

講義ノートの設計と現行の査読手順。`reading-book` の 7 本のスクリプトは移植せず、fuzukue の deck・カード・レビュー台帳に寄せて作り直す。

`course_plan`、`course_next`、`course_commit` で節を書き進め、`request_review` で保存済みの節を査読する。原本の変更判定は、節のカードと関連する TeX の実バイトから求める `book_version` を使う。

## 方針

- 取り込んだ本の deck は原本として触らない。講義は別 deck（`kind = "notes"`）にする。
- 講義のカードは普通のカードにする。編集、ビルド、既存の文体検査とレビューをそのまま使う。
- 状態は 1 か所（講義 deck の `course.toml`）にだけ持つ。カードの有無は状態の根拠にしない。

## 1. 節と講義ノートの対応

講義 deck 直下に `course.toml` を置く。

```toml
book = "books/my-book"          # 原本 deck（プロジェクト相対）
book_fingerprint = "…"          # 計画を作った時点の原本全体の指紋

[[section]]
key = "ch2-sec1"                # 原本 structure.yaml の節キー。講義側の一意な鍵
status = "todo"                 # todo | adopted | skipped | writing | written | done
book_fingerprint = "…"          # この節の原本カード群の指紋（source_fingerprint を連結して再計算）
cards = ["lec-ch2-sec1"]        # この節から作った講義カード ID
exercises = { "ex-2-3" = "adopt", "ex-2-4" = "skip" }
```

- 節の鍵は原本の `structure.yaml` の節キー。講義カードには `from: <book>#<節キー>` と、書いた時点の `source_fingerprint` を付ける（既存フィールド）。
- 原本が後から変わったかは、節の `book_fingerprint` と現在値の比較で分かる。`label_diff.py` の役目は指紋比較で足りる。差があれば `stale` として一覧に出し、状態は自動で動かさない。
- 取り込み済みの旧い本にも使える。原本側に追加のファイルや項目は要らず、`course.toml` がなければ講義がないだけ。

## 2. 進捗の保存

- 場所は `course.toml` の `status` と、トップレベルの `next`（次に書く節の鍵）。別の進捗ファイルは作らない。
- 書き込みは一時ファイルに書いて rename する。節 1 件の更新は 1 回の書き換えで済ませる。
- 経路は新しい MCP ツール `course_plan`（節の採否を設定）、`course_next`（次の節を返し `writing` にする）、`course_commit`（書き終えを記録）の 3 つに絞る。CLI は同じ関数を呼ぶ。

## 3. 中断と再試行で重複しない仕組み

節の鍵ごとに冪等に動かす。

1. `course_next`: `adopted` の先頭を `writing` にして返す。すでに `writing` の節があればその節を返す（新しい節は始めない）。
2. 講義カードの ID は節の鍵から決める（例 `lec-<節キー>`）。ID が固定なので、再実行は新規作成ではなく既存カードの上書きになり、複製が出ない。
3. カードをすべて書き終えて、文体検査が通ってから `course_commit` で `written` にし、`cards` と書いた時点の `book_fingerprint` を保存する。
4. 再開は `course.toml` を読むだけ。`writing` の節は、そこまでに書けたカードを残したまま続きから書く。`written` 以降の節には触れない。
5. 同じ `course_commit` を再送しても、状態と `cards` が同じなので何も変わらない。

壊れた状態の扱い: `written` なのにカードが無い、または `writing` で `cards` にないカードが残っている場合は、`course_next` が不整合として報告して止まる。自動では直さない。

## 4. 数学・出力の確認の依頼と記録

- **依頼**: `request_review` に `kind`（`math`、`output`、`both`）と、`course_commit` で保存した順序の `cards` を渡す。必要に応じて `section` に節キーを渡す。`both` は同じ時点の写しから両方の査読を準備する。
- **数学と出力**: 数学査読は対象カードと参照先を調べる。出力査読は私用の写しで TeX を組み立て、PDF・ビルド診断・入力記録を利用する。どちらも指摘を付けられるのは対象カードだけ。 [査読の依頼](delegate.md#デッキの節を査読する) · [参照先の取得](graph.md#節の数学査読で辿る参照)
- **記録**: サーバーの状態ディレクトリに査読セッションごとの結果を保存する。`course.toml` は節ごとに `version`、`requested_at`、`status` と、依頼を照合する `request` を持つ。`request` には `session`、共通の `reviewed_version`、査読ごとの `input_hash` を記録する。指摘はコメント下書きに入り、指摘ゼロの結果も保存される。
- **完了**: 数学と出力が両方とも現在の入力に対して `clean` なら、`course_plan` で `done` に進める。節の版や記録されたワークスペース内の入力が変わると、該当する査読を `stale_reviews` に表示する。再起動後の `course_plan` でも、保留中の依頼と保存済みの結果を照合する。

## 5. 受け入れ確認（端から端まで）

小さな LaTeX の本（3 節以上、演習あり）を使う。

1. `import-latex` で取り込み、`course_plan` で節を採否に振り分ける。`course.toml` ができ、原本 deck は 1 バイトも変わらない。
2. `course_next` で 1 節目を取り、カードを書き、`course_commit` する。`status = "written"`、カードに `from` と指紋が付く。
3. 2 節目の途中（カードを 1 枚書いた後）でプロセスを止める。新しいプロセスで `course_next` を呼ぶと、同じ 2 節目を返し、新規の節は始まらない。残りを書いて commit すると、カードの ID に重複がなく、1 節目は変わらない。
4. `course_commit` を同じ引数で再送しても、`course.toml` が変わらない。
5. 数学と出力の確認をまとめて依頼する。両者の結果が同じ `reviewed_version` を持ち、異なる `input_hash` と共に記録される。
6. 講義カードを 1 枚編集すると、その節の確認が「古い」と表示され、`done` にできない。再確認後は `done` にできる。
7. 原本の節を編集すると、その節が `stale` と表示される。状態は変わらない。
8. 旧い取り込み済みの本（`course.toml` なし）でも、既存の編集・ビルド・deck 全体のレビューが従来どおり動く。
9. `cargo test` の関連スイートと、`course.toml` の読み書き・不整合検出の単体テストが通る。

## 決めたいこと

- 最初の試験に使う本はどれか。
- 元スクリプトは移植せず作り直す前提でよいか。
