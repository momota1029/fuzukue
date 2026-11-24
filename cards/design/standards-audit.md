---
id: standards-audit
kind: plan
title: これまでの設計と、既にある標準・定番の道具の突き合わせ（2026-11-21）
status: open
links:
  - {to: lean-reading, rel: see}
---

> いままでのやり方でこういう標準がありそうなことは調べておいてほしい

今後は設計の前に「既にあるもの」を調べ、カードに書く。ここでは、これまでに作った・設計したものを洗い直した結果をまとめる。〇は既に標準に沿っているもの、△は合わせたほうがよいもの。

## 既に沿っているもの（〇）

- 言語サーバー: LSP（[[code-reading]]）。Lean は `lake serve` の `$/lean/plainGoal`。
- 拡張機能: WebAssembly の Component Model と WIT（[[extensions]]）。
- スキル: Agent Skills の `SKILL.md`（YAML の前書きに `name`・`description`）。仕様は agentskills.io/specification。fuzukue のスキルとルールパックはこの形を使う。Claude Code 独自の項目は他の道具では無視される。
- エージェントへの指示: `AGENTS.md`。
- 文言の翻訳: Fluent（[[localisation]]）。
- PDF と原稿の対応: SyncTeX。
- 差分: unified diff と git の三方向マージ（[[branch-review]]）。
- 文献: BibTeX（[[references]]、[[sources-ledger]]）。

## 合わせたほうがよいもの（△）

1. **文体の検査**（[[style-lint]]、[[term-lint]]）: 日本語技術文書と AI ライティングの textlint プリセットとの対応を表にした。文長・読点の既定値を 100 字・3 個にそろえ、文体の混在、句点、連続する漢字・助詞、二重否定、弱い表現、冗長な表現などをネイティブで検査する。prh YAML の読み書き、取り込み、例による検証に対応した。ワークスペースに設定と実行環境があれば外部の textlint も実行し、規則と位置が重複する診断をまとめる。カード表示・自動査読・MCP に規則名と修正の目安を出す。
2. **GitHub の見張り**（issue-threads、作業中）: GitHub の REST API の推奨は、ETag を使う条件付きの要求（304 は主な使用量に数えない）、`x-poll-interval` を守る、同じ引数で問い合わせる、の三つ。サーバーは Tailscale の内側にあり公開の受け口がないので、webhook ではなく見張り（polling）にする。→ 作業中の Sol に指示済み。
3. **ゴミ箱**（[[workspaces]] の閉じる・消す）: デスクトップには freedesktop.org の Trash 仕様（`~/.local/share/Trash` の `files/` と `info/*.trashinfo`）がある。今はサーバー独自のゴミ箱。→ サーバーの中の取り消しには今のままでよいが、ワークスペースのファイルを消す時は OS のゴミ箱（Linux は Trash 仕様、Windows はごみ箱）に入れる案を検討する。
4. **自分の入れ替え**（[[self-update]]）: Linux では systemd のソケット引き継ぎ（socket activation、`sd_listen_fds`）と、ユーザーのサービス（`Restart=on-failure`）が定番。WSL も systemd を使える。今は自前の監督プロセスと待ち受けの張り直し。→ systemd が使える時はユーザーのサービスとして動かし、待ち受けをソケットで引き継ぐ。使えない時は今の方法。
5. **配布の確かさ**（[[distribution]]）: 更新の配布には The Update Framework（TUF）や、署名の minisign・Sigstore がある。今は SHA-256 の照合だけで、署名は「配る範囲が広がる時」としていた。→ 友人（Mac）に配る段階で、minisign 程度の署名を入れる。
6. **テスト結果の読み取り**（[[code-reading]] の段階 2）: 多くのテスト道具が JUnit XML を書き出せる（pytest の `--junitxml`、cargo の `cargo2junit` など）。今は各道具の出力を読んでいる。→ 道具が JUnit XML を出せる時はそれを優先して読む。
7. **記号の索引**（[[symbols]]）: コードの索引には SCIP（以前の LSIF）がある。→ LSP から作る索引を保存する時の形として検討する（段階 4）。
8. **注釈・コメント**（[[review-loop]]、査読の指摘）: 文書の一部に付けるコメントには W3C の Web Annotation データモデル（対象の選び方: 引用＋前後の文、位置）がある。→ カードの文に付けるコメントの位置の持ち方を、この形（TextQuoteSelector）に寄せると、本文が変わっても付け直しやすい。
9. **文芸的 Lean**（[[lean-reading]]）: Verso の文芸的モードと Lean Blueprint。→ カードに反映済み。
10. **スライド**（[[slides-and-documents]]）: Markdown のスライドには Marp や reveal.js がある。→ 作る時に確かめる。

## 進め方

- 1・2・9 は効き目が大きい。1 は文体査読の手前に入れ、2 は作業中のものに入れた。
- 3〜8 は、その機能に次に手を入れる時に一緒に直す。
