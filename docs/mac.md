# Mac で使う

Mac では、サーバー（secretaire）とクライアント（GPUI）を同じ Mac で動かす。

## 入れるもの

```sh
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# GitHub CLI と、PDF の寸法を読む poppler（pdfinfo）
brew install gh poppler
gh auth login
# TeX（synctex を含む）。数式の多い本を PDF にするなら MacTeX
brew install --cask mactex-no-gui
# エージェント（それぞれ自分のアカウントでログインする）
#   Claude Code: https://code.claude.com/docs/en/setup
#   Codex: https://developers.openai.com/codex
```

クライアントのビルドには macOS SDK とコンパイラを含む Command Line Tools が要る（`xcode-select --install`）。GPUI はアプリの起動時に Metal シェーダーをコンパイルする。導入手順は [Mac のセットアップガイド](install-mac.md) を参照。

## 本を始める

```sh
git clone "$FUZUKUE_REPO_URL"   # 公開ソースのリポジトリ URL を事前に設定
fuzukue/scripts/new-book.sh ~/my-book
cp 原著.pdf ~/my-book/books/original.pdf
fuzukue/scripts/run.sh ~/my-book
```

- 左ペインの「📖 通して読む」で、structure.yaml の順にカードを読める。数式も描かれる。
- カードの「原著 p.N」で、原著の該当ページが右に開く。
- 「編集」で、カードをその場で直せる。
- 真ん中のチャットで Claude に頼む（`/codex …` で Codex に直接）。例: 「原著の p.12–20 を読んで、第1章のカードを起こして」。
- 本を GitHub のリポジトリにすると、章ごとの Issue を作業場として回せる（docs/workshop.md）。

## 確かめていないこと（2026-11-19）

サーバーの部分は macOS 向けにコンパイルが通ることを確かめた（`cargo check --target aarch64-apple-darwin`）。
クライアントの Mac でのビルドと動作は、まだ確かめていない。動かなかったら、エラーをそのまま知らせてほしい。
