---
id: theme-chocolate-mint
kind: design
title: チョコミントのアプリ配色
status: proposed
links:
  - {to: appearance, rel: refines}
  - {to: android-desktop-parity, rel: refines}
files:
  - crates/fuzukue-view/src/appearance.rs
  - crates/fuzukue/src/appearance.rs
  - android/app/src/main/java/dev/labgui/remote/RemoteUi.kt
  - crates/fuzukue-reading/src/assets/proof.css
  - vendor/gpui-pdf/src/lib.rs
---

## 方針

デスクトップ・Android・ブラウザー向けの閲覧画面に、共通の配色の役割を持たせる。ダークの地色はチョコレート `#403034`、主操作色はミント `#02FAC8`。ライトは A（淡い青 `#E6F1FA`）、B（淡い青ミント `#E3F8F3`）、C（ごく淡い青 `#F1F7FC`）の 3 候補を比較する。本文は全案で `#403034`。リンクと操作は深いミント `#00735E` を基本案、ミルクチョコ `#6B4A3A` を比較案にする。ライトの鮮やかなミントは選択行やバッジの面に使い、チョコ文字と、必要に応じて輪郭やチェックを重ねる。成功は黄色みのあるオリーブ、警告は橙、エラーはローズ、情報は青とし、ラベルや記号でも区別する。

## 現状

- Desktop の `crates/fuzukue-view/src/appearance.rs` は面と境界 6 色 × 2（12 行）。`crates/fuzukue/src/appearance.rs` は既定色・アクセント・色変換など 29 行（UI 17、テスト 12）。ほかに `math/glyphs.rs` 6 行は変換、`server.rs` 1 行は Windows フラグ。撮影マスク、PDF の重ね色、HTML の固定色は別口。探索ツリーは `crates/fuzukue/src/exploration.rs` の GPUI テーマ利用。
- `crates/fuzukue/Cargo.lock` の `gpui-component 0.10.0` は `gpui-kit` 経由の依存。中央の `src/theme/default-theme.json` に色リテラル 193 個。手元の `vendor/gpui-base/src/theme_tokens.rs` に 36 行（18 役 × 2）、Base の他ファイルに 130 行（変換・テストも含む）。`vendor/gpui-pdf/src/lib.rs` はフォールバック 6 行と重ね色 6 行。
- Android の `android/app/src/main/java/dev/labgui/remote/RemoteUi.kt` に 18 色引数（12 一致行）、同じディレクトリの `Composer.kt` に透明色 1 行、`android/app/src/main/res/drawable/ic_notification.xml` に白 1 行。Compose が OS の明暗を拾う。独自の `colors.xml` は見当たらない。
- `crates/fuzukue-reading/src/assets/` 内の色リテラルは `companion-template.html` 62、`prose-page-template.html` 43、`review-page-template.html` 41、`proof.css` 50、`section.css` 47。追加の埋め込み色は `src/proof/html.rs` 2、`src/review/diagram.rs` 11、`src/review/standalone.rs` 3、`src/section/original.rs` 2。同じく `crates/fuzukue/src/source/lean.rs` 3、`crates/secretaire/src/harness_html.rs` 3。ブラウザー配布の PDF には TeX の文書色もある。
- Rust は `rgb(` / `rgba(` / `hsl(` / `hsla(`、6・8 桁の `0x…`、`Hsla::` / `Rgba::` / `Color::` がある行を数えた。HTML/CSS は 3・4・6・8 桁の `#…` と同じ色関数の出現数。

## トークン

| 面のトークン | ダーク | A · 淡い青 | B · 淡い青ミント | C · ごく淡い青 |
| --- | --- | --- | --- | --- |
| `base` 地 | `#403034` | `#E6F1FA` | `#E3F8F3` | `#F1F7FC` |
| `chrome` クローム | `#37292D` | `#DCEAF5` | `#D8F1EA` | `#E7F0F7` |
| `surface` カード・会話 | `#49373C` | `#F0F6FC` | `#EEF9F6` | `#F8FBFE` |
| `raised` 浮き面 | `#533F45` | `#FFFFFF` | `#FFFFFF` | `#FFFFFF` |
| `code` コード | `#2E2326` | `#D7E7F4` | `#CFEDE5` | `#DAE9F5` |

| 役割のトークン | ダーク | ライト A / B / C 共通 |
| --- | --- | --- |
| `text` 本文 | `#F9ECEE` | `#403034` |
| `secondary` 補助 | `#DEC8CD` | `#5D4A50` |
| `disabled` 無効 | `#AD969C` | `#85747A` |
| `divider` 区切り | `#806970` | `#A69AA0` |
| `outline` 強い枠 | `#B49BA2` | `#80717B` |
| `primary` 主操作の塗り | `#02FAC8` | `#00735E` |
| `hover` 主操作のホバー | `#61FDDC` | `#005F50` |
| `active` 主操作の押下 | `#00DDB1` | `#004B40` |
| `on-primary` 主操作上の文字 | `#403034` | `#FFFFFF` |
| `link` リンク | `#7AFFE1` | `#00735E` |
| `link-hover` リンクのホバー | `#AAFFEC` | `#005F50` |
| `link-active` リンクの押下 | `#02FAC8` | `#004B40` |
| `alternate` ミルクチョコの塗り／リンク | — | `#6B4A3A` |
| `alternate-hover` ミルクチョコのホバー | — | `#583A2C` |
| `alternate-active` ミルクチョコの押下 | — | `#4A3024` |
| `focus` フォーカス | `#02FAC8` | `#00735E` |
| `selection` 選択行 | `#24594F` | `#02FAC8` |
| `on-selection` 選択行の文字 | `#F9ECEE` | `#403034` |
| `text-selection` 文字範囲の選択 | `#235349` | `#BDF0E4` |
| `mint-fill` 鮮やかなミントの塗り | `#02FAC8` | `#02FAC8` |
| `on-mint` 鮮やかなミント上の文字 | `#403034` | `#403034` |
| `success` 成功 | `#C9E477` | `#435817` |
| `success-bg` 成功の面 | `#344019` | `#E7F1CA` |
| `warning` 警告 | `#FFC08A` | `#824008` |
| `warning-bg` 警告の面 | `#503421` | `#FCE9D6` |
| `error` エラー | `#FFA7B3` | `#952B49` |
| `error-bg` エラーの面 | `#522C38` | `#FBE5EA` |
| `info` 情報 | `#B9D0FF` | `#294C8A` |
| `info-bg` 情報の面 | `#2F3C58` | `#E3EDFF` |

差分の追加は `success` / `success-bg`、削除は `error` / `error-bg` と同色の専用ロール。選択行は `focus` の輪郭と `on-selection` のチェックを併用。進行表示の弧は `focus`、進行中の文言は `text` または `secondary`。

## コントラスト

比率はすべて `x:1`。通常テキストの AA は **4.5:1** 以上、**×** は未達。表示だけ小数第 2 位に丸め、判定は丸め前。無効な部品の文字は WCAG の例外に当たる場合も比率を示した。純ミントの前景行は誤用例で、ライトで許可する用途は塗りのみ。

| ライトの地 | A · #E6F1FA | B · #E3F8F3 | C · #F1F7FC |
| --- | --- | --- | --- |
| `#403034` 本文 | 10.84 | 11.23 | 11.51 |
| `#00735E` リンク | 5.07 | 5.25 | 5.38 |
| `#6B4A3A` ミルクチョコの塗り／リンク | 6.87 | 7.12 | 7.30 |
| `#02FAC8` 鮮やかなミントを文字に流用 | **1.18 ×** | **1.22 ×** | **1.25 ×** |

### 通常面に置く文字

#### ダーク

| 前景 ＼ 背景 | `base` | `chrome` | `surface` | `raised` | `code` |
| --- | --- | --- | --- | --- | --- |
| `text` | 10.80 | 12.03 | 9.64 | 8.44 | 13.19 |
| `secondary` | 7.84 | 8.73 | 6.99 | 6.12 | 9.57 |
| `disabled` | 4.51 | 5.02 | **4.02 ×** | **3.52 ×** | 5.50 |
| `link` | 10.19 | 11.35 | 9.09 | 7.95 | 12.44 |
| `link-hover` | 10.78 | 12.01 | 9.62 | 8.42 | 13.16 |
| `link-active` | 9.18 | 10.22 | 8.19 | 7.17 | 11.20 |
| `success` | 8.78 | 9.78 | 7.83 | 6.86 | 10.72 |
| `warning` | 7.78 | 8.67 | 6.94 | 6.08 | 9.50 |
| `error` | 6.76 | 7.53 | 6.03 | 5.28 | 8.26 |
| `info` | 8.00 | 8.92 | 7.14 | 6.25 | 9.77 |

#### A · 淡い青

| 前景 ＼ 背景 | `base` | `chrome` | `surface` | `raised` | `code` |
| --- | --- | --- | --- | --- | --- |
| `text` | 10.84 | 10.13 | 11.41 | 12.42 | 9.84 |
| `secondary` | 7.16 | 6.69 | 7.53 | 8.20 | 6.49 |
| `disabled` | **3.84 ×** | **3.59 ×** | **4.04 ×** | **4.40 ×** | **3.48 ×** |
| `link` | 5.07 | 4.74 | 5.34 | 5.81 | 4.60 |
| `link-hover` | 6.66 | 6.22 | 7.01 | 7.63 | 6.04 |
| `link-active` | 8.81 | 8.23 | 9.27 | 10.09 | 7.99 |
| `alternate` | 6.87 | 6.43 | 7.24 | 7.88 | 6.24 |
| `alternate-hover` | 8.91 | 8.33 | 9.38 | 10.21 | 8.08 |
| `alternate-active` | 10.54 | 9.85 | 11.09 | 12.07 | 9.56 |
| `success` | 6.92 | 6.47 | 7.29 | 7.93 | 6.28 |
| `warning` | 6.82 | 6.38 | 7.18 | 7.82 | 6.19 |
| `error` | 6.74 | 6.30 | 7.09 | 7.72 | 6.11 |
| `info` | 7.35 | 6.87 | 7.73 | 8.42 | 6.66 |
| `mint-fill` | **1.18 ×** | **1.10 ×** | **1.24 ×** | **1.35 ×** | **1.07 ×** |

#### B · 淡い青ミント

| 前景 ＼ 背景 | `base` | `chrome` | `surface` | `raised` | `code` |
| --- | --- | --- | --- | --- | --- |
| `text` | 11.23 | 10.47 | 11.55 | 12.42 | 10.00 |
| `secondary` | 7.41 | 6.91 | 7.62 | 8.20 | 6.60 |
| `disabled` | **3.98 ×** | **3.70 ×** | **4.09 ×** | **4.40 ×** | **3.54 ×** |
| `link` | 5.25 | 4.89 | 5.40 | 5.81 | 4.68 |
| `link-hover` | 6.90 | 6.43 | 7.09 | 7.63 | 6.14 |
| `link-active` | 9.13 | 8.50 | 9.38 | 10.09 | 8.12 |
| `alternate` | 7.12 | 6.64 | 7.32 | 7.88 | 6.34 |
| `alternate-hover` | 9.23 | 8.60 | 9.49 | 10.21 | 8.22 |
| `alternate-active` | 10.92 | 10.17 | 11.22 | 12.07 | 9.71 |
| `success` | 7.17 | 6.68 | 7.37 | 7.93 | 6.38 |
| `warning` | 7.07 | 6.59 | 7.27 | 7.82 | 6.29 |
| `error` | 6.98 | 6.50 | 7.18 | 7.72 | 6.21 |
| `info` | 7.61 | 7.09 | 7.82 | 8.42 | 6.77 |
| `mint-fill` | **1.22 ×** | **1.14 ×** | **1.26 ×** | **1.35 ×** | **1.09 ×** |

#### C · ごく淡い青

| 前景 ＼ 背景 | `base` | `chrome` | `surface` | `raised` | `code` |
| --- | --- | --- | --- | --- | --- |
| `text` | 11.51 | 10.77 | 11.96 | 12.42 | 10.03 |
| `secondary` | 7.60 | 7.11 | 7.90 | 8.20 | 6.62 |
| `disabled` | **4.07 ×** | **3.81 ×** | **4.23 ×** | **4.40 ×** | **3.55 ×** |
| `link` | 5.38 | 5.04 | 5.60 | 5.81 | 4.69 |
| `link-hover` | 7.07 | 6.61 | 7.35 | 7.63 | 6.16 |
| `link-active` | 9.35 | 8.75 | 9.72 | 10.09 | 8.15 |
| `alternate` | 7.30 | 6.83 | 7.59 | 7.88 | 6.36 |
| `alternate-hover` | 9.46 | 8.85 | 9.83 | 10.21 | 8.24 |
| `alternate-active` | 11.18 | 10.47 | 11.63 | 12.07 | 9.75 |
| `success` | 7.35 | 6.88 | 7.64 | 7.93 | 6.41 |
| `warning` | 7.24 | 6.78 | 7.53 | 7.82 | 6.31 |
| `error` | 7.15 | 6.69 | 7.44 | 7.72 | 6.23 |
| `info` | 7.80 | 7.30 | 8.10 | 8.42 | 6.80 |
| `mint-fill` | **1.25 ×** | **1.17 ×** | **1.30 ×** | **1.35 ×** | **1.09 ×** |

### 操作・選択・状態の面に置く文字

| 前景 / 背景 | ダーク | ライト共通 |
| --- | --- | --- |
| `on-primary` / `primary` | 9.18 | 5.81 |
| `on-primary` / `hover` | 9.82 | 7.63 |
| `on-primary` / `active` | 7.09 | 10.09 |
| `on-primary` / `alternate` | — | 7.88 |
| `on-primary` / `alternate-hover` | — | 10.21 |
| `on-primary` / `alternate-active` | — | 12.07 |
| `on-selection` / `selection` | 6.98 | 9.18 |
| `link` / `selection` | 6.58 | **4.29 ×** |
| `alternate` / `selection` | — | 5.82 |
| `text` / `text-selection` | 7.59 | 9.91 |
| `secondary` / `text-selection` | 5.51 | 6.54 |
| `disabled` / `text-selection` | **3.17 ×** | **3.51 ×** |
| `link` / `text-selection` | 7.16 | 4.63 |
| `link-hover` / `text-selection` | 7.58 | 6.08 |
| `link-active` / `text-selection` | 6.45 | 8.05 |
| `alternate` / `text-selection` | — | 6.28 |
| `alternate-hover` / `text-selection` | — | 8.14 |
| `alternate-active` / `text-selection` | — | 9.63 |
| `success` / `text-selection` | 6.17 | 6.33 |
| `warning` / `text-selection` | 5.47 | 6.24 |
| `error` / `text-selection` | 4.75 | 6.16 |
| `info` / `text-selection` | 5.63 | 6.71 |
| `on-mint` / `mint-fill` | 9.18 | 9.18 |
| `success` / `success-bg` | 7.84 | 6.74 |
| `warning` / `warning-bg` | 7.10 | 6.61 |
| `error` / `error-bg` | 6.44 | 6.43 |
| `info` / `info-bg` | 7.09 | 7.14 |
| 純ミント文字 / 白（誤用例） | **1.35 ×** | **1.35 ×** |

ライトの選択行では深いミントのリンク文字をチョコ文字に切り替え、下線は維持する。文字範囲の選択はグリフの色が残るため、別の `text-selection` を使う。ダークのコードの構文色は別途実描画で測定する。

### 輪郭・フォーカス・選択の境界

下表は非テキストの **3:1** を基準にした比較。隣接面の行は 5 面との最小値とその面を示す。区切りは装飾専用。選択の塗りだけで判別しにくい面は別の輪郭やチェックで示す。フォーカスと主操作の塗りが直接触れる箇所は地色の間隔を設け、フォーカス色を周囲の面に接するように描く。文字範囲の選択は標準の選択操作と文字の可読性を優先し、色差の小さい面での位置の見つけやすさを実機で確認する。

| 前景 / 背景 | ダーク | A · 淡い青 | B · 淡い青ミント | C · ごく淡い青 |
| --- | --- | --- | --- | --- |
| `outline` / 隣接面 | 3.77 (`raised`) | 3.64 (`code`) | 3.70 (`code`) | 3.71 (`code`) |
| `focus` / 隣接面 | 7.17 (`raised`) | 4.60 (`code`) | 4.68 (`code`) | 4.69 (`code`) |
| `divider` / 隣接面 | **1.93 ×** (`raised`) | **2.14 ×** (`code`) | **2.18 ×** (`code`) | **2.19 ×** (`code`) |
| `primary` / 隣接面 | 7.17 (`raised`) | 4.60 (`code`) | 4.68 (`code`) | 4.69 (`code`) |
| `hover` / 隣接面 | 7.66 (`raised`) | 6.04 (`code`) | 6.14 (`code`) | 6.16 (`code`) |
| `active` / 隣接面 | 5.53 (`raised`) | 7.99 (`code`) | 8.12 (`code`) | 8.15 (`code`) |
| `alternate` / 隣接面 | — | 6.24 (`code`) | 6.34 (`code`) | 6.36 (`code`) |
| `alternate-hover` / 隣接面 | — | 8.08 (`code`) | 8.22 (`code`) | 8.24 (`code`) |
| `alternate-active` / 隣接面 | — | 9.56 (`code`) | 9.71 (`code`) | 9.75 (`code`) |
| `selection` / 隣接面 | **1.21 ×** (`raised`) | **1.07 ×** (`code`) | **1.09 ×** (`code`) | **1.09 ×** (`code`) |
| `text-selection` / 隣接面 | **1.11 ×** (`raised`) | **1.01 ×** (`code`) | **1.01 ×** (`code`) | **1.01 ×** (`code`) |
| `focus` / `primary` | **1.00 ×** | **1.00 ×** | **1.00 ×** | **1.00 ×** |
| `focus` / `hover` | **1.07 ×** | **1.31 ×** | **1.31 ×** | **1.31 ×** |
| `focus` / `active` | **1.30 ×** | **1.74 ×** | **1.74 ×** | **1.74 ×** |
| `focus` / `alternate` | — | **1.36 ×** | **1.36 ×** | **1.36 ×** |
| `focus` / `alternate-hover` | — | **1.76 ×** | **1.76 ×** | **1.76 ×** |
| `focus` / `alternate-active` | — | **2.08 ×** | **2.08 ×** | **2.08 ×** |
| `focus` / `selection` | 5.93 | 4.29 | 4.29 | 4.29 |

### 算出方法

不透明な sRGB の前景と背景を対象にした。チャンネルを 0–1 にして線形化し、相対輝度を `0.2126 R + 0.7152 G + 0.0722 B`、比を `(明 + 0.05) / (暗 + 0.05)` で計算した。半透明や影が入る描画は、実際の隣接面で合成後の前景・背景を別途測る。[WCAG 2.2 – 1.4.3 Contrast (Minimum)](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html) · [1.4.11 Non-text Contrast](https://www.w3.org/WAI/WCAG22/Understanding/non-text-contrast.html) · [1.4.1 Use of Color](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color.html)

```sh
python3 -I - '#403034' '#E6F1FA' <<'PY'
import sys

def luminance(color):
    def linear(c):
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    r, g, b = (linear(int(color[i:i+2], 16) / 255) for i in (1, 3, 5))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b

low, high = sorted(map(luminance, sys.argv[1:]))
ratio = (high + 0.05) / (low + 0.05)
print(f'{ratio:.2f}:1', 'AA' if ratio >= 4.5 else '×')
PY
```

## 実装時の対応

| 役割 | GPUI Component / Base | Android Material 3 / Compose |
| --- | --- | --- |
| 地・面・コード | `base` → `background`、`chrome` → `sidebar` / `title_bar`、`surface` → `secondary` / `muted`、`raised` → `popover`。会話・コードは `DesktopColors` → `ThemeSurfaces` を使い、コードの背景とエディターの構文背景の組も揃える。Base の `surface` は `popover` から投影される。 | `base` → `background` / `surfaceContainerLowest`、`chrome` → `surfaceContainerLow` / `surfaceDim`、`surface` → `surface` / `surfaceContainer`、`raised` → `surfaceContainerHigh` / `surfaceContainerHighest` / `surfaceBright`。コードは専用のロールで保持する。 |
| 文字 | `foreground`、`sidebar_foreground`、`popover_foreground` → 本文。 `muted_foreground` → 補助文字。無効色を実際に無効な UI に渡す。 `secondary_foreground` → `secondary` 上に必要な本文色。 | `onBackground` / `onSurface` → 本文、`onSurfaceVariant` → 補助、無効色はコンポーネントの disabled 引数・トークンで指定。 `Surface` に必要な `contentColor` を渡す。 |
| 枠・フォーカス | `sidebar_border` / `title_bar_border` → 区切り、`border` / `input` → 強い枠、`ring` / `drag_border` → フォーカス。ダークの `input_background()` は `input` を透過合成するので、入力面を明示するか実描画を再計測する。 | `outlineVariant` → 区切り、`outline` → 強い枠。フォーカス時の枠/リングとカーソルを操作色トークンで渡す。単色のリングが主ボタンと接するときは地色の間隔を設ける。 |
| 主操作・リンク | `primary{,_hover,_active,_foreground}` と必要な `button_primary*`、`link{,_hover,_active}` を別々に投影する。現在のリンク色を流用する `accent_button`・スピナーは、それぞれ主操作・進行表示へ接続する。 | `primary` / `onPrimary` → ボタン、テキストのリンクはアプリ側の役割として `Markdown.kt` に渡す。ライトのミルクチョコ案はボタンとリンクの両方を切替可能な対応として示す。実際の押下・リップル合成を計測する。 |
| 選択 | メニュー・行は `accent` / `accent_foreground` と `list_active` / `list_active_border`、`sidebar_accent*`。文字範囲は `selection`（Base に投影）。ライトの純ミント面上では本文色かミルクチョコでラベルを描く。 | `primaryContainer` / `onPrimaryContainer` → 選択行。選択済み行や自分の発話にある明示的な `primary` 文字指定は `onPrimaryContainer` 等へ。 `LocalTextSelectionColors` の背景に範囲選択、ハンドルにフォーカス色。 |
| 成功・警告・エラー・情報 | `success` / `warning` / `danger` / `info` は既存の文字呼び出しに合う前景。面付きの表示には専用の面と前景の組を渡す。 `button_{success,warning,danger,info}*` と、Base の `destructive` / `destructive_foreground` を使う部品では面上の文字を確認する。 | `error` / `onError`、`errorContainer` / `onErrorContainer` にエラー対（明暗を反転する組も同じ 2 色）。成功・警告・情報は追加トークン。 `tertiary` に意味が違う既存表示があるので、警告の割当は呼び出し側で明示する。 |

GPUI は明暗のロード後にアプリ側のトークンを `Theme::update` で反映し、Component の描画用トークンと Base の両方を同期する。Compose は `secondary` / `tertiary`、inverse 系、`surfaceTint` と既定の合成も確認する。[Compose Material 3 ColorScheme](https://developer.android.com/reference/kotlin/androidx/compose/material3/ColorScheme)

Web では同じ役割の CSS カスタムプロパティを共通のスタイルに定義し、明暗を適用する。淡い地と濃いアクセントを使う文書出力は、配布・印刷時の配色として扱う。

## テーマパック

チョコミントを含む既存のプリセットと利用者が配布するテーマに、この配色表の役割と明暗の組を使う。デスクトップ・Android・Web は同じテーマファイルの値をそれぞれの描画に反映する。 [R-67](../../roadmap/cards/R-67.md)

## 移行順

1. 現行の明暗を基準に、デスクトップの会話・枠・状態色、Android の `RemoteTheme`、Web の CSS/SVG を順に意味トークンへ接続する。既存の色で小さく分けて移行し、半透明は合成先とセットで把握する。
2. Desktop の既定値に新パレットを適用する。システム追従・ライト・ダーク、明暗切替、既存 TOML の読み込みを確認する。旧 TOML に保存されたアクセント値を引き継ぎ、新規・未指定の設定に新しい既定色を割り当てる。保存された既定値から利用者の選択履歴を判断することはできない。利用者指定色のコントラスト補正は維持する。
3. A/B/C と 2 種類の操作色を比較してライトの採用値を決める。同じ役割を Compose と Web に渡す。ネイティブ PDF の外枠と注釈を別の差分で移行し、Web の依存図と査読ラベルも確認する。
4. Linux の `scripts/shot.sh` でチャット・探索・差分・エディター・紙面/PDF・査読を明暗で比較する。Windows のフォーカスとシステム追従、Android のシステムバー・キーボード・無効状態、ブラウザーの明暗と印刷も確認する。

## 回帰しやすい箇所

| 面 | 確認項目 |
| --- | --- |
| 構文ハイライト・数式 | `Theme.highlight_theme`、`chat_code.rs`、`composer_markdown.rs`、LSP の独立した配色を画面の地に合わせて測る。コメント・選択中の色付きグリフ・検索結果・アクティブ行・ガターを含める。 |
| 差分 | `unified_diff.rs` と `workspace_git.rs` は追加・削除を `success` / `danger` の文字で表示する。背景付きプレビューは同じ前景と状態面を使う。差分用の役割を独立させ、`+` / `-` を保持。 |
| PDF | `PdfStyle` は枠・ヘッダー・プレースホルダーに適用。PDF のピクセルは原稿の配色を保持する。検索・選択・リンク・注釈は文書の白地と色付き図版の上で描画を確認する。 |
| チャート | `crates/fuzukue/src/thread_info.rs` のコンテキスト使用量バーは 50%・80% を境に成功・警告・エラーの色を `theme.secondary` の上に描く。割合の文字表示を保ち、各バーとトラックを確認する。Component の `chart_1`〜`chart_5` / `chart_grid` と Web の依存図は、系列色・グリッド・ラベルを別々に点検する。 |
| 翻訳のずれ | `translation.rs` と `deck_overview.rs` の小さい警告は通常テキストの 4.5:1 で判定。`Stale` / `Unverified` / `SourceUnavailable` の文言を優先する。 |
| 査読指摘 | Desktop の `review_findings.rs` は重大度を現状ラベル中心で表示し、Web の `proof.css` と TeX は別々に色を持つ。重大度と対応状況を別々に扱い、色を割り当てる画面ではラベルとアイコンを併用する。 |
| フォーカス・選択・派生色 | GPUI の入力面、PDF の透過色、Compose の無効アルファ・リップル、CSS の選択は最終的に見える色で再測定する。選択行は高コントラストの輪郭・チェック、リンクは下線を付ける。 |
