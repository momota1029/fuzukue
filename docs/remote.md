# リモート接続

リモート接続には Tailscale を推奨します。secretaire とクライアントを同じ tailnet に参加させ、Tailscale の IP アドレスに WebSocket と HTTP で接続します。端末に Tailscale を入れずに使う場合は、下記の HTTPS プロキシを使います。

## サーバー側

サーバーを動かす環境で `tailscale status` を実行してサーバーと接続元の IP、許可する端末のログイン名を確認します。次を `~/.config/fuzukue/server.toml` に保存し、例示用の IP とログイン名を、サーバーと利用者の実際の値に置き換えます。

```toml
[remote]
listen = ["127.0.0.1:7878", "192.0.2.21:7878"]
allowed_logins = ["user@example.com"]
```

`FUZUKUE_ADDR`（例 `127.0.0.1:7878,192.0.2.21:7878`）を設定すると `listen` を上書きします。指定がなければ既定は `127.0.0.1:7878` です。`FUZUKUE_ALLOWED_LOGINS` はカンマ区切りで指定でき、設定ファイルの許可リストに追加されます。設定ファイルの場所は `$XDG_CONFIG_HOME/fuzukue/server.toml` または `FUZUKUE_SERVER_CONFIG` でも指定できます。

リポジトリのルートから起動します。すでに同じポートで secretaire が動いている場合は、そのプロセスを終了してから起動し直します。

```sh
cargo build --release -p secretaire
RUST_LOG=info ./target/release/secretaire /path/to/project
```

Tailscale の IPv4・IPv6 範囲から来た接続は、`tailscale whois --json <接続元IP>` の `UserProfile.LoginName` が許可リストにあれば通ります。照会結果は IP ごとに最大 3 分間保持します。WSL でサーバーを動かすユーザーからこのコマンドを実行できることを確認してください。[Tailscale CLI の whois](https://tailscale.com/docs/reference/tailscale-cli#whois)

## クライアント側

GPUI の「設定 → サーバー → リモート」で、例の `192.0.2.21` を実際の Tailscale IP に置き換えた `ws://192.0.2.21:7878/ws` を入力し、「保存して再接続」を押します。許可リストで通る端末はトークン欄を空にできます。IPv6 の例は `ws://[2001:db8::21]:7878/ws` です。リモートモードでは設定欄の URL が使われます。

手で設定する場合は、Windows の `%APPDATA%\fuzukue\settings.toml`、macOS の `~/Library/Application Support/fuzukue/settings.toml` に次を記述します。
Windows で `fuzukue.exe` の隣に `settings.toml` を置くと、ポータブルモードとして `%APPDATA%` より優先され、設定画面からの変更もそのファイルに保存されます。

```toml
[server]
mode = "remote"
url = "ws://192.0.2.21:7878/ws"
# token = "発行したトークン"
```

Windows から接続を切り分けるときは、PowerShell で `Test-NetConnection 192.0.2.21 -Port 7878` を実行します。GPUI が「接続済み」になったら、会話、カードの関連ファイル、PDF を開きます。認証だけを試すときは、許可リストにない端末でトークンなしの接続が拒否され、正しいトークンを入力すると接続とファイル取得が通ることを確認します。

## トークンで接続する

許可リストに入っていない端末や HTTPS プロキシ経由の接続には、端末ごとのトークンを使えます。サーバー起動ユーザーで、起動時と同じプロジェクトのルートから実行します。別のディレクトリから実行する場合は `--project /path/to/project` を付けます。

```sh
secretaire device add phone
secretaire device list
secretaire device revoke phone
```

`add` はトークンを一度だけ表示します。`--expires-at 2027-01-01T00:00:00Z` で有効期限を指定できます。`list` には ID・名前・作成時刻・最終認証時刻・有効期限が表示され、`revoke` は名前と ID のどちらでも指定できます。トークンは状態ディレクトリにソルト付きハッシュで保存され、権限は `0600` です。最終認証時刻の更新は端末ごとに最大 1 分に 1 回です。失効した端末トークンで認証済みの WebSocket は、失効から 5 秒以内に切断されます。

従来の共有トークンも既定で使えます。共有トークンのファイルは状態ディレクトリに権限 `0600` で自動生成されます。`[remote].allow_shared_token = false` で共有トークンによる認証を無効にできます。

```sh
./target/release/secretaire token /path/to/project
./target/release/secretaire token --rotate /path/to/project
```

GPUI はトークンを設定画面で伏せ字にし、WebSocket とファイルの HTTP リクエストに `Authorization: Bearer <token>` を付けます。トークンはユーザーの設定ファイルに保存されるため、そのファイルの共有やコミットは避けてください。ヘッダーを指定できないクライアント向けに、WebSocket のアップグレードだけ `/ws?token=...` も受け付けます。URL は履歴やプロキシのログに残ることがあるため、ヘッダーを優先してください。共有トークンを再発行すると、次の接続やファイル取得から旧トークンが無効になります。共有トークンで接続済みの WebSocket は接続を維持します。

## Tailscale なしで公開する

HTTPS を終端するプロキシを secretaire と同じマシンで動かします。先に端末トークンを発行し、サーバーを次のように設定します。

```toml
[remote]
listen = ["127.0.0.1:7878"]
trusted_proxy = ["127.0.0.1"]
allow_shared_token = false
local_listen = "127.0.0.1:7879"
```

`7878` はプロキシ専用です。プロキシが検証した接続元 IP を `X-Forwarded-For` の末尾に置き、元の方式を `X-Forwarded-Proto: https` で伝える必要があります。ローカルのクライアントと `/mcp` は別の `7879` を使います。`local_listen` を省くとローカル用の空きポートを自動で選びます。Tailscale を併用する場合は `listen` に Tailscale の IP とポートを追加します。

- **Cloudflare Tunnel** — 公開ホスト名のサービスを `http://127.0.0.1:7878` に設定し、公開ホスト名では HTTPS を使います。[Tunnel の設定](https://developers.cloudflare.com/tunnel/features/locally-managed-tunnels/configuration-file/) · [転送ヘッダー](https://developers.cloudflare.com/fundamentals/reference/http-headers/)
- **Caddy + Let's Encrypt** — 公開 DNS を Caddy に向け、以下を Caddyfile に設定します。[自動 HTTPS](https://caddyserver.com/docs/automatic-https) · [リバースプロキシ](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)

```caddyfile
work.example.com {
    tls {
        ca https://acme-v02.api.letsencrypt.org/directory
    }
    reverse_proxy 127.0.0.1:7878
}
```

クライアントには `wss://work.example.com/ws` と端末トークンを設定します。ファイル取得も HTTPS を使います。secretaire のポートを直接公開したり、ローカル専用ポートを転送したりしないでください。プロキシで認証済みのレスポンスをキャッシュしない設定にしてください。

## WSL、ファイアウォール、公開範囲

- Tailscale を WSL 内で動かす場合は、WSL の Tailscale IP に直接バインドします。tailnet のアクセスルールと WSL のファイアウォールでは、接続させる端末からの TCP 7878 のみを通します。
- Tailscale が Windows 側だけで動く構成では、WSL のミラーモードと必要な Hyper-V ファイアウォール規則を確認します。転送で Tailscale の接続元 IP が置き換わる構成では、HTTPS プロキシとトークンを使います。[Microsoft Learn の WSL ネットワーク解説](https://learn.microsoft.com/ja-jp/windows/wsl/networking)
- LAN から使う場合も、LAN から届く HTTPS プロキシに接続します。ループバックと Tailscale の範囲以外で、`trusted_proxy` を設定せずに待ち受けようとすると起動時に拒否されます。
- `/mcp` は直接のループバック接続専用です。ローカル専用ポートは Tailscale Serve や Funnel 等でも転送しないでください。
