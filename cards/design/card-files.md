---
id: card-files
kind: design
title: カードは TeX と Lean がくっついたファイルの束。画面はチャットが主役
status: accepted
links:
  - {to: card-model, rel: refines}
  - {to: workspace-tabs, rel: refines}
  - {to: pdf-to-card, rel: refines}
files: [crates/fuzukue-cards/src/lib.rs, crates/secretaire/src/build.rs, crates/secretaire/src/lean.rs, crates/secretaire/src/comments.rs, crates/fuzukue/src/main.rs]
---
## カードの形
```
cards/thm/sphere-concentration/
  card.md        # frontmatter（id, kind, status, requires, source, lean）とノート
  statement.tex  # 主張（論文はこれをそのまま \input する）
  proof.tex      # 証明
```
- `.tex` が本物。表示（数式つき）も論文の組み立ても同じファイルから。1 ファイルの `.md` カードもそのまま使える。
- Lean は `card.md` の `lean:` に宣言名を書く。サーバーが対応する Lake パッケージをビルドして
  `#print axioms` を読み、宣言ごとに 証明済み（標準の公理だけ）/ sorry / 見つからない / ほかの公理 を出す。
  宣言名の先頭が `Pkg` なら既定は `lean/<Pkg>/` で、監査時は `import Pkg` を使う。
  プロジェクト直下の `fuzukue.toml` で `[lean]` の下に `packages = { Yulang = "lean" }` と書けば、
  `Yulang` の宣言をプロジェクト相対の `lean/` で検査する。import 先も指定する場合は
  `packages = { Yulang = { path = "lean", imports = ["Yulang", "Yulang.EOFRoleInference"] } }` と書く。
- 論文の組み立て: `structure.yaml` の順に `.tex` を `\input` した `build/paper.tex` を uplatex + dvipdfmx（synctex つき）で PDF に。
  synctex はカードの `.tex` そのものを指すので、PDF からカードへの逆引きが正確になる。

## Writing harness の論文

ユーザー「本来は論文編集のはずです」

- `deck.toml`、`cards.toml`、`harness.toml` の順で設定を探す。writing-harness の事実カードは Markdown の `## Statement` と `## Proof` を使い、説明カードは著者の TeX 本文を使う。論文編集画面では `structure.yaml` の順にカードと小節を表示する。
- 論文エディタの保存は事実カードの主張・証明を元の Markdown に反映し、frontmatter と `Shortcuts`・`Notes` などを保持する。説明カードは frontmatter を保持して本文を保存する。
- PDF は `[build].tex` のターゲット名でデッキ内の `build/` に出力する。省略時は `cards.tex`、部分原稿は `{out = "part1.tex", sections = 3}` を使う。各カードの組版用 TeX を `\input` し、節・小節ラベル、定理環境、`ref_command`、執筆者の証明や後置きの証明を反映する。指定プリアンブルと節構成から毎回組み立てるので、`paper/` のファイルを入力として扱う必要はない。
- latexmk のエンジンはプリアンブルの `% !TeX program` に従う。日本語の既定は upLaTeX + dvipdfmx、`bibliography` は `amsplain` と upBibTeX で処理する。SyncTeX の逆引きは生成時のソースマップでカード ID・Markdown パス・行番号に戻す。
- `build/` は生成物。別リポジトリでデッキを管理する場合も `build/` を gitignore に登録する。PDF の生成と逆引きに必要な TeX Live・Poppler のコマンドはサーバー側に置く。

## 既存の TeX 原稿を組み立てる

MCP の `build_paper` や画面の「PDF を作る」で既存の原稿を使うには、デッキの設定に次のように書く。

```toml
[build]
tex_root = "manuscript/main.tex"
engine = "lualatex"
latexmk_extra = ["-time"]
```

- `tex_root` はデッキ内の既存の `.tex` ファイルへの相対パス。指定したファイルの親フォルダーで latexmk を実行する。latexmk はそのフォルダーの `.latexmkrc` を通常どおり読み、成果物はデッキの `build/` に原稿と同じベース名で置く。
- `engine` は `uplatex-dvipdfmx`、`lualatex`、`pdflatex`、`xelatex` のいずれか。指定すると既存のエンジン選択より優先する。`tex_root` を指定して `engine` を省くと upLaTeX + dvipdfmx を使う。
- `latexmk_extra` に渡せる引数は `-file-line-error`、`-g`、`-norc`、`-quiet`、`-recorder`、`-silent`、`-time`、`-verbose`、`-Werror`。
- `build_paper` はプロジェクト相対の `tex_root` と `pdf`、エンジン、ログのヘッダと末尾を返す。直近の実行ログは失敗時も `log_path` が示す `build/fuzukue-latexmk-output.txt` から読める。設定の誤りで実行できない場合は、呼び出し結果に理由が出る。
- この 3 項目を削除すると、従来の生成ルートとエンジン選択に戻る。

## 画面
- 左: 研究の縦タブと、探す（Ctrl+K / ⌘K）・概要・論文編集・右を閉じる。下に接続と使用量。
- 真ん中: チャット（主役）。返答の `[[カード]]` を押すとそのカードが右に開く。エージェントがカードのファイルを書き換えると、そのカードが自動で開く。
- 右（必要なときだけ）: カード 1 枚（Lean の状態つき）、PDF、論文編集、Issue、ノート、概要（以前の左ペイン）。
- 論文中心の配置（2026-11-19）: 左の「⇄ 論文を真ん中に」で、真ん中に論文編集、右にチャット（「▸ 右」で PDF に切り替え）。
  ユーザー:「論文の編集画面を真ん中にも置けるようにするのはどうでしょうか．真ん中のほうが作業をしやすいこともあるので」
- 論文編集: カードを論文の順に並べ、カードごとに「＋ コメント」でためる。「コメントを送信（n 件）」でタブの会話に 1 通で送り、送ったら消す。
  ためているコメントは `<デッキ>/.fuzukue/comments.json`（git には入れない）。「PDF を作る」「PDF を開く」も。

TeX と Lean のファイルをカードに紐づける。チャットの隣でカードや PDF を確認し、ためたコメントを一括で送信できるようにする。

確かめた（2026-11-19）: サンプルの論文の組み立て（1.4 秒）、組み立てた PDF からカードへの逆引き、Lean の判定（証明済み・sorry）、
コメントを 2 件ためて一斉送信 → 会話に 1 通 → 消える。送ったコメントを受けた Claude が実際に、球面の集中の証明の段 3 を式つきに書き直し、
Lean の三角不等式を（長さが同じという仮定を足して）証明した。数学と Lean の判定は監督役が確かめた。
確かめていない: ポップアップ（別ウィンドウ）はまだ作っていない。手での操作の細部。
