---
id: android-desktop-parity
kind: design
title: Android で PC 版と同じ操作感にする
status: proposed
links:
  - {to: android-client, rel: refines}
  - {to: appearance, rel: see}
  - {to: localisation, rel: see}
  - {to: notifications, rel: see}
---

2026-11-23「アンドロイド版がPC版っぽくない」。指示・返信・承認の流れを揃え、画面遷移は端末の幅に合わせる。

## 優先順｜揃える 8 項目

1. **揃える（影響 大／工数 中）** 会話の見た目。PC は返信を背景に直接置き、利用者の発言だけ右寄せの吹き出しにする。Android は両方を丸い枠に入れ、「あなた」「返信」を毎回出す。PC は無彩色の面と線、Android は紫系 Material 3 の既定書体と Canvas 描画の入力欄アイコン。文脈ごとの色・角丸・余白・文字階層・線画アイコンを揃える。 [PC の会話](../../crates/fuzukue/src/chat.rs) · [PC の配色](../../crates/fuzukue-view/src/appearance.rs) · [Android のテーマ](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt)
2. **揃える（影響 大／工数 中）** ツール・作業役の行。PC は「ツール N 件」を折りたたみ、展開時に入出力を読める。作業役の発話も既定で折りたたむ。Android はツール名・短い引数・成否を 1 件ずつ出し、委譲内のツールを省き、作業カードに最新要約か最終報告を出す。件数・状態・失敗の強調と展開の段階を揃える。無言判定も合わせる。Android は空白と `<silent/>` を隠すが、PC は `<silent />` や HTML コメントだけの返信も隠す。 [PC の行と表示判定](../../crates/fuzukue/src/chat.rs) · [PC の行計測](../../crates/fuzukue/src/chat_transcript.rs) · [Android の行](../../android/app/src/main/java/dev/labgui/remote/Timeline.kt) · [無言判定](../../android/app/src/main/java/dev/labgui/remote/AssistantSilence.kt)
3. **揃える（影響 大／工数 中）** 承認の位置と結果。PC はツール行の文脈に承認を置き、判断済みの許可・拒否・自動判断を折りたたんで残す。Android は会話末尾に別カードを出し、解決イベントで消す。保留中の承認へすぐ移動できる導線を保ち、依頼元と結果を会話に残す。 [PC の承認行](../../crates/fuzukue/src/chat.rs) · [Android の会話](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt) · [解決処理](../../android/app/src/main/java/dev/labgui/remote/RemoteRepository.kt)
4. **揃える（影響 大／工数 小）** 日英の用語。PC は Fluent、Android は独立した XML。「ワークスペース」「ピン留め」「許可／拒否」は一致するが、入力欄は「メッセージを入力／メッセージ」、返信通知は「返信が届きました。／応答が終了」。Android 内でも「方針／プラン」が混在する。共通画面の文言を一つのカタログから供給する。 [PC のチャット文言](../../locales/ja/chat.ftl) · [PC の通知文言](../../locales/ja/notifications.ftl) · [Android の文言](../../android/app/src/main/res/values/strings.xml)
5. **揃える（影響 中／工数 小）** スレッド一覧の目印。PC は共通の線画アイコンと「ピン留め」「スレッド」の見出しで区切る。Android はピン留め優先の一列で、行内の文字が目印。モバイルのワークスペース選択を生かし、区分・選択中・承認待ちが同じ意味で目に入る行にする。 [PC のサイドバー](../../crates/fuzukue/src/sidebar.rs) · [スレッド一覧](../../crates/fuzukue/src/threads.rs) · [アイコン](../../crates/fuzukue/src/icons.rs) · [Android の一覧](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt)
6. **揃える（影響 中／工数 小）** 入力欄から作業役の承認方針を確認する。両方に添付・送信・停止のある丸い入力欄がある。PC は直下に作業役の方針を表示し、Android は接続設定で変更できる。Android でも現在値と設定への入口を入力欄の近くに置く。 [PC の入力欄](../../crates/fuzukue/src/chat.rs) · [Android の入力欄](../../android/app/src/main/java/dev/labgui/remote/Composer.kt) · [Android の設定](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt)
7. **揃える（影響 中／工数 中）** 方針の状態と見出し。PC のスレッド情報は完了条件・進み具合・確認待ち・履歴を示す。Android の下から開くシートは目標・状態・質問・次の手順・支障理由を表示し、会話上の要約は任意。完了条件と進捗、確認待ちの見せ方を先に揃える。 [PC の方針](../../crates/fuzukue/src/thread_info.rs) · [Android の方針](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt)
8. **揃える（影響 中／工数 中）** 通知の設定範囲。両方が同じ通知種別を受け、対応する OS ではスレッドを開ける。PC は表示中の抑制・スレッド別ミュート・作業役の通知を設定でき、Android は表示中の抑制が固定で作業役の通知は全体設定。スレッド別の設定を揃え、音やバナーは OS の仕組みを使う。 [PC の通知設定](../../crates/fuzukue/src/notifications.rs) · [Android の通知判定](../../android/app/src/main/java/dev/labgui/remote/RemoteRepository.kt) · [通知サービス](../../android/app/src/main/java/dev/labgui/remote/ConnectionService.kt)

## 端末差と未実装

1. **端末差（そのままでよい）** PC はワークスペース切替・メニュー／ツリー・会話・右ペインを持つ。Android はスマホで一覧→会話、720 dp 以上で二列。添付は PC の貼り付け・ドロップ・画面撮影と、Android の写真・ファイル・カメラ・共有・IME 画像入力が端末に合う。通知も Android は前景サービスと OS チャネルを使い、条件を満たす Bash 承認は通知で操作できる。 [PC のメニュー](../../crates/fuzukue/src/menu_bar.rs) · [ファイルツリー](../../crates/fuzukue/src/file_browser.rs) · [Android の入力欄](../../android/app/src/main/java/dev/labgui/remote/Composer.kt)
2. **未実装** Android は質問の通知を受けるが、PC の質問カード（選択・自由入力・回答結果）と回答送信がない。方針シートの「質問」は表示のみ。 [PC の質問カード](../../crates/fuzukue/src/chat_question.rs) · [Android のイベント処理](../../android/app/src/main/java/dev/labgui/remote/Protocol.kt)
3. **未実装** PC のスレッド別モデル・推論強度と入力欄下の承認モードが Android にない。Android の設定にあるワークスペース共通の作業役承認方針は別の操作。 [PC の設定操作](../../crates/fuzukue/src/chat.rs) · [Android の設定](../../android/app/src/main/java/dev/labgui/remote/RemoteUi.kt)
4. **未実装** Android にスレッドのアーカイブ閲覧やデッキ概要（カード一覧・Issue）への入口がない。明暗は OS に追従し、PC のテーマ指定・UI／本文／コード別フォント・文字サイズ・アプリ内言語選択に相当する項目がない。 [PC のデッキ概要](../../crates/fuzukue/src/deck_overview.rs) · [外観設定](../../crates/fuzukue/src/appearance.rs)
5. **未実装** Android の簡易 Markdown は数式を RaTeX で描画し、失敗時は TeX を表示する。PC のチャットにあるスレッド別数式マクロと Markdown リンクのクリック操作は Android にない。 [PC の Markdown](../../crates/fuzukue/src/chat.rs) · [Android の Markdown](../../android/app/src/main/java/dev/labgui/remote/Markdown.kt)

## 進め方

1. 共通の用語と配色・文字階層・余白・線画アイコンを定義し、会話・一覧・入力欄から適用する。
2. 同じ会話データで、返信・ツールの折りたたみ・無言・保留／判断済み承認の行を揃える。PC と Android のライト／ダークを同じ場面で撮る。
3. 質問へ会話内で答えられるようにする。入力欄から作業役の承認方針へ、スレッド情報からモデル・承認モード・通知設定へ進めるようにし、方針シートに進捗を出す。

## 確認範囲

1. 作業ツリーの実装、[PC の撮影シナリオ](../../tests/shots)、[Android の AppShotTest](../../android/app/src/test/java/dev/labgui/remote/AppShotTest.kt) を確認。`tests/` に PNG はなく、Android の既存 PNG は `android/app/build/shots/app.png`（52×24 の旧ロゴ表示）のみ。現行画面の画像比較と再撮影は未実施。
2. Android 実機なし。日本語 IME、添付／カメラ、通知の到達・操作、実機の数式・文字サイズ、接続／復帰は未確認。

## フォルダーの読み取り許可

v65 のフォルダーアクセス依頼は、両端末の会話内で既存の承認カードに表示する。Android では 1 時間の許可または拒否を選べる。期間の選択、許可の追加と取り消し、読み取り履歴の表示は現在デスクトップのスレッド詳細で行う。 [フォルダーの読み取り許可](../../docs/folder-grants.md)
