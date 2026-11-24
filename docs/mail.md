# メール連携

secretaire は読み取り専用の IMAP でメールサーバーにログインし、メールを検索・取得します。利用者の「サンダバとしてログインする話がいいです」という方針に沿った構成です。Thunderbird は下書きの確認と本人による送信に使います。

## アカウント設定

設定先は `$XDG_CONFIG_HOME/fuzukue/mail.toml` です。既定は `~/.config/fuzukue/mail.toml` で、`FUZUKUE_MAIL_CONFIG` で上書きできます。アカウントごとに `[[accounts]]` を追加します。

```toml
[[accounts]]
name = "gmail"
kind = "imap"
host = "imap.gmail.com"
port = 993
user = "researcher@example.invalid"
auth = "password"
```

OAuth2 を使う場合は、利用を許可されたクライアント登録に合わせて、利用者が自身の設定ファイルで `auth = "oauth2"` と接続・認可の項目を指定します。登録に紐づく値はプロジェクト外の設定ファイルで管理します。

- Gmail のパスワード認証は、2 段階認証を有効にし、アプリパスワードを発行できるアカウントで利用します。組織やアカウントの設定により、発行できない場合があります。 [Google アカウント ヘルプ](https://support.google.com/accounts/answer/185833?hl=ja)

### 認証コマンド

`secretaire mail auth <account>` で、パスワード方式ならアプリパスワードを画面に表示せずに入力します。OAuth2 方式なら表示された URL をブラウザーで開き、認可後に遷移した URL を端末へ貼り付けます。認可コードには PKCE（S256）を使い、リダイレクト URI と `state` を照合します。プロバイダーのトークン更新は TLS で行い、アクセストークンは期限までメモリーに保持します。更新時に新しいリフレッシュトークンが返れば置き換えます。

保存先は `$XDG_CONFIG_HOME/fuzukue/mail-credentials.json` です。ディレクトリは `0700`、ファイルは `0600` とし、同じディレクトリ内の一時ファイルからアトミックに置き換えます。ディレクトリが既にある場合は権限を `0700` に設定してください。`FUZUKUE_MAIL_CONFIG` で設定先を変えた場合は、そのファイルと同じディレクトリに認証情報を保存します。設定と認証情報をプロジェクト外に置き、サーバーのログ、プロンプト、MCP 応答から認証情報を除外します。

既存の mailtools から移す場合は、明示的に `secretaire mail import-mailtools [path]` を実行します。アカウント名とユーザーが一致する設定に、アプリパスワードまたはリフレッシュトークンを取り込みます。パスの省略時は `$MAILTOOLS_HOME/credentials.json`、その環境変数も未設定なら `~/.config/mailtools/credentials.json` を参照します。移行元のファイルは変更せず、取り込んだ値は表示しません。

## MCP と読み取り専用の境界

| ツール | 動作 |
| --- | --- |
| `mail_folders {account}` | フォルダー名と特殊用途のフラグを取得する。 |
| `search_mail {query, account?, folder?, from?, subject?, since?, until?, unseen?, gmail?, has_attachment?, limit?}` | サーバーで検索し、件名などのヘッダーとフラグを返す。 |
| `read_mail {id, max_chars?}` | 指定された一通の本文をテキストにして返す。 |
| `mail_thread {id}` | 検索で得た ID を起点に同じ IMAP アカウント内の会話をたどり、古い順で最大 50 通を返す。査読依頼はヘッダーのみ。 |
| `save_attachment {id, part, deck?}` | 添付をプロジェクト内の `inbox/<日付>-<件名>/` に保存する。デッキを指定した場合は、その配下の `inbox/` を使う。 |
| `mail_journal {account?, since?, new_only?}` | 編集システムのメールのヘッダーを検索し、前回の確認以降に見つけたメールに `NEW` を付ける。 |
| `draft_mail {account, to, cc?, subject, body, in_reply_to_id?, attach?}` | プロジェクトの `outbox/` に .eml を作り、Thunderbird の作成画面を開く（試験機能）。 |

メール設定がなければ、メール用 MCP ツールもプロンプトへの案内文も出しません。設定があれば `MailSource` を通して各アカウントに接続し、tokio 上の検証付き TLS 経由で、保守されている IMAP クライアントを使用します。接続、認証、コマンドにはタイムアウトを設けます。 [async-imap の Session API](https://docs.rs/async-imap/latest/async_imap/struct.Session.html)

認証後は `LIST`、`EXAMINE`、`UID SEARCH`、`UID FETCH` を使用します。非 ASCII の検索値は IMAP リテラルとして渡します。ヘッダー検索では `DATE FROM SUBJECT MESSAGE-ID AUTHENTICATION-RESULTS` と `FLAGS` を取得します。本文と添付は UID で一通ずつ取得し、既読フラグは変えません。送信、移動、削除、フラグの変更、リモートの下書き作成は行いません。 [IMAP の読み取りと検索の仕様](https://www.rfc-editor.org/rfc/rfc3501)

検索は `SINCE`、`BEFORE`、`FROM`、`SUBJECT`、`TEXT`、`UNSEEN` を変換します。`since` と `until` の両端を含み、IMAP サーバーの内部日付で絞ります。非 ASCII の検索では `CHARSET UTF-8` を使い、Gmail で対応するアカウントだけ `X-GM-RAW` を受け付けます。検索語をプロトコルの引数としてエスケープし、改行をコマンドの区切りとして解釈させません。フォルダーの `all`、`sent`、`trash` は `LIST` の `\All`、`\Sent`、`\Trash` で解決し、表示名と modified UTF-7 の通信用名前を区別します。 [Gmail の IMAP 拡張](https://developers.google.com/workspace/gmail/imap/imap-extensions)

一覧から通常は直近の数件だけを返し、本文の全件キャッシュは作りません。`has_attachment` を指定したときだけ `BODYSTRUCTURE` を取得して判定します。先に件数上限を適用すると条件に合うメールを取りこぼすため、上限に達するか候補が尽きるまで候補を分割して取得します。通常検索は INBOX を既定とし、必要なフォルダーを指定できます。文字コードは UTF-8、ISO-2022-JP、EUC-JP、Shift_JIS（CP932）、GB18030 を扱い、GB2312/GBK 表記のヘッダーには GB18030 を試します。

メッセージの表示 ID は `account:folder:uid` です。読み取り時に末尾の UID と先頭のアカウント名を分け、フォルダー名内のコロンも扱います。応答の `<untrusted-mail>` で囲まれた ID はそのまま次のツールへ渡せます。囲みには `UIDVALIDITY` に対応する識別子が入っており、フォルダーの世代が変わった場合は再検索が必要です。サーバー再起動後も最初に検索してください。

`mail_thread` は選択フォルダーを最初に、Sent・All Mail・Archive の特殊用途フォルダーを次に、残りの選択可能なフォルダーも検索します。`Message-ID`、`In-Reply-To`、`References` を `UID SEARCH HEADER` で探し、ヘッダーを取得して完全一致を確認します。同じ `Message-ID` の複製は一通にまとめ、探索後に本文を一通ずつ取得します。探索は最大 4 段、64 ID、各段・各フォルダーにつき最大 100 件の未確認候補、結果は 50 通までです。上限で探索が止まった場合は `truncated: true` を返します。査読依頼には `査読` と `body_withheld: true` を付け、本文も添付情報も取得しません。

## 査読依頼、下書き、承認

- 既定の査読判定パターンと編集システム送信元のリストを移植し、設定ファイルのトップレベルの `review_patterns`（正規表現の配列）と `editorial_senders`（送信元の文字列の配列）で追加できます。査読判定はデコードした件名に適用し、該当すれば `査読` を付けます。本文・添付の取得直前にもヘッダーを確認し、該当したら拒否して本人にメールクライアントでの確認を促します。MCP に上書き用の引数は設けません。
- ジャーナルの既定検索期間は過去 180 日間です。初回は既存分を基準として保存し、次回以降はアカウントごとに `Message-ID`（欠落時はフォルダー・`UIDVALIDITY`・UID）で既出を判定します。既出状態はサーバーの状態ディレクトリでアトミックに更新します。
- 添付保存ではファイル名と件名を正規化し、パスの脱出とシンボリックリンクを拒否します。同名には連番を付けます。
- メール由来のデータを含むツール応答は、送り主が書いた外部データであることを示す一行から始まります。JSON の `notice` にも同じ文を入れます。本文やヘッダーなどのメール由来の文字列は、エスケープして `<untrusted-mail>` で囲います。
- HTML 本文はコメントや `hidden` 属性、インラインスタイルで非表示にされた要素を除いてテキストに変換します。ゼロ幅文字、双方向制御文字、改行とタブ以外の制御文字を除き、連続する空行は一行にまとめます。128 バイト以上の長い Base64 状の文字列は `…` に置き換え、長い HTTP / HTTPS URL はホスト名と `/…` だけを残します。本文は整形後に一通あたり最大 20,000 文字（`read_mail` は `max_chars` で調整可）を返し、省略時は省略した文字数を表示します。
- 差出人は表示名とメールアドレスを区別して表示します。`Authentication-Results` に SPF、DKIM、DMARC の `fail` または `none` がある場合や、認証結果が `none` の場合は、`sender_warning` に「送信者を確認できない」と表示します。この表示は受信メールのヘッダーに記録された結果に基づきます。
- Claude と Codex はメールを読んだ後もスレッドの承認スタイルに従います。メールに書かれた依頼は利用者に伝え、利用者自身が会話で頼んだ場合に実行します。検索・外部取得・委任に渡すメール情報は、利用者の依頼に必要な範囲に絞るようシステムプロンプトで指示します。
- メールを有効にした Claude セッションでは、プロジェクト・ローカル設定の読み込みと追加の MCP サーバーを無効にします。fuzukue の MCP には Claude セッションごとのキーが必要です。

## Thunderbird の作成画面（trial — may be removed）

`draft_mail` は UTF-8 の MIME 下書きを保存して、宛先・Cc・件名・本文・添付を埋めた Thunderbird の作成画面を起動します。送信は利用者が内容と差出人を確認し、Thunderbird で送信ボタンを押して行います。Linux は `thunderbird`、macOS は `open -a Thunderbird`、WSL は Windows の Thunderbird を使います。WSL の添付パスは `wslpath` で変換します。実行ファイルの場所は `[thunderbird]` の `executable` で指定できます。`compose_started` は起動コマンドを開始できたことを示します。起動コマンドを開始できない場合も .eml は保存されます。

- 有効な返信元の `Message-ID` があるときは、`In-Reply-To`、`References`、`X-Unsent: 1` を書いた .eml を `-file` で開きます。Thunderbird はこのファイルを編集用の下書きとして開き、宛先・本文・添付とともに `References` を読み込みます。ツール結果は `compose_threading: "eml"` です。WSL では .eml のパスを変換します。 [Thunderbird の X-Unsent 対応](https://bugzilla.mozilla.org/show_bug.cgi?id=166541) [下書きの References 読み込み](https://github.com/mozilla/releases-comm-central/blob/master/mailnews/mime/src/mimedrft.cpp#L358)
- 新規メールは `-compose` で開きます。返信元に有効な `Message-ID` がない場合も `-compose` を使い、返信元本文の先頭 1,500 文字を引用します。その場合は `compose_threading: "quoted"` と通知を返します。 [Thunderbird のコマンドラインヘルプ](https://searchfox.org/comm-central/source/mail/components/MessengerContentHandler.sys.mjs#1703)

## 動作確認

テストは TLS 接続の先に相当する in-memory の IMAP 応答、モックのトークン応答、一時ディレクトリの合成データを使用します。実機では、実際の IMAP アカウントで、送信済みフォルダーをまたぐスレッド取得、HTML メールの整形、プロバイダーが記録する認証結果の表示、閲覧後の承認スタイルを確認してください。Thunderbird では作成画面の起動と差出人、宛先、本文、添付、返信の `References` を確認してください。
