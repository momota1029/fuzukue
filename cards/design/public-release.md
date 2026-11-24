---
id: public-release
kind: design
title: fuzukue の公開計画
status: proposed
links:
  - {to: release-distribution, rel: refines}
  - {to: self-update, rel: see}
  - {to: distribution, rel: see}
  - {to: remote-access, rel: see}
---

公開リポジトリへの移行は未着手。公開版のサーバー更新と認証は以下の設計に沿って整える。

## 公開するもの

- 最初の公開はソースのみ。fuzukue の自作部分には MIT または Apache-2.0 を適用する。公開履歴は新規の初回コミットから始める。
公開前に候補のファイル一覧、ライセンス、第三者表示と自己更新を確認する。稼働版のコミットが公開履歴にない場合も更新と復旧を検証する。

## 公開版の更新

- サーバーは GitHub のタグ／リリースから更新候補を取得し、タグが指す完全なコミット SHA と配布マニフェストの SHA が一致することを確認する。成果物はマニフェストの SHA-256 で照合し、マニフェストの署名検証も導入したい。
- 更新が見つかったら利用者に通知し、利用者が「インストール」を押したときだけ更新を始める。開発者向けには手元のソースからビルドする経路を残す。入れ替えとロールバックは既存の [[self-update]] の処理を使う。
- クライアントは接続先サーバーの版に追従する。サーバーが互換性を確認したクライアント成果物を配布する（[[distribution]]）。

## 接続と認証

- Tailscale を推奨の既定経路とし、README ではこの接続手順を先に案内する。Tailscale の導入自体は必須にしない。
- 認証処理は tailnet の ID（WHOIS）を照合する方式と、トークンを照合する方式に分ける。tailnet 外でトークン認証を使う場合は TLS を必須にし、その用途でループバック以外に平文 HTTP を設定していたら起動時に拒否する。

## Android ベータ

- ローリング更新するプレリリース `android-beta` に、署名済み APK、`.sha256`、`android-latest.json` を置く。APK とハッシュを先に揃え、更新情報の JSON を最後に差し替える。
- 署名とハッシュを照合し、実機でインストールと更新を確認する。配布手順は [Android の README](../../android/README.md) を参照。
