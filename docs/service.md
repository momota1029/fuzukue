# サーバーをサービスとして動かす

macOS と Linux でクローンからサーバーとクライアントをまとめて導入するには、[セットアップスクリプト](../README.ja.md#ソースから起動する) を使う。macOS では LaunchAgent、Linux では配布インストーラーと共通の `fuzukue-release.service` を登録する。Linux 側のログは `journalctl --user -u fuzukue-release.service` で見る。クローンの更新後はセットアップを再実行する。

以下は `secretaire service` が管理する Linux 専用の `fuzukue.service` の手順。

Linux / WSL2 で `secretaire` を systemd ユーザーサービスにすると、クライアントを開く前からサーバーを動かせる。
起動時に指定したプロジェクトを開き、クライアントがつながっていなくても自動査読、Issue スレッド、長期目標などの処理が続く。
その間も利用枠を消費し、利用枠が 95% を超えたときの作業停止は引き続き適用される。

```sh
secretaire service install --project /path/to/project
secretaire service install --project /path/to/project --now
secretaire service status
secretaire service restart
secretaire service stop
secretaire service uninstall
```

`install` は `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/fuzukue.service` を作り、有効化する。
`--project` を省くと現在のディレクトリを使う。`--now` を付けるとその場で起動する。
手動起動中のサーバーがポートを使っている場合は、起動元で Ctrl+C を押すか、PID を確認して `kill <PID>` で終了する。
`--force` は事前のポート確認を省く。`uninstall` は起動中のサービスを停止してユニットを削除する。

`status` では有効化、稼働状態、TCP 接続、linger を確認できる。
ログアウト後もユーザーマネージャーを動かすには `loginctl enable-linger "$USER"` を使う。
ログは `journalctl --user -u fuzukue.service` で見る。

`secretaire service stop`、`systemctl --user stop fuzukue.service`、SIGTERM、SIGINT は、実行中の仕事に退勤を指示して新規開始を止める。保存済みの保留入力と再開地点は次回起動時に読み直す。退勤を待つ上限は `server.toml` で指定でき、既定値は 60 秒。二度目のシグナルで直ちに終了する。

```toml
[shutdown]
drain_seconds = 60
```

`service install` が書くユニットの `TimeoutStopSec` は `drain_seconds + 15` 秒（既定 75 秒）。上限を変更したら `service install` でユニットも更新する。`KillMode=mixed` は、最初の SIGTERM をサーバー本体に送り、子プロセスが退勤中に結果を保存できるようにする。

systemd 254 以降と cgroup v2 では、導入スクリプトがサーバーをユニット配下の `server` に置く。エージェント、テスト、ビルド、レシピは個別の `job-*` cgroup で動き、終了時には残った子プロセスも停止する。ジョブごとの既定上限は 8 GiB で、`FUZUKUE_JOB_MEMORY_MIB` で変更できる。レシピにはレシピ自身のメモリ上限を適用する。

ユニット全体のメモリにはジョブとページキャッシュが含まれる。サーバー単体の使用量は `server/memory.current`、匿名メモリとファイルキャッシュの内訳は `server/memory.stat` の `anon` と `file` で確認できる。cgroup の委譲が使えない環境ではプロセスグループで停止する。

自己更新は作業が空くまで待ち、起動先を切り替えて systemd で再起動する。
新しいビルドの応答を確認できなければ旧版に戻し、自己更新の状態に結果を残す。
既存の `server-builds/current` がサービス対応前の版を指す場合は、サービス化の前に通常の自己更新で対応版まで進める。
更新元を自動で見つけられない場合は、`server.toml` の `[self_update]` に `source` を指定する。

WSL2 では systemd を有効にしてサービスを登録し、普段どおり Windows のクライアントを開く。
サービスは WSL が稼働している間に動く。Windows のログオン時に `wsl.exe -d <distro> --exec /bin/true` を実行するだけでは常駐を保証できない。
Windows の `.wslconfig` に `[wsl2]` の `vmIdleTimeout=-1` を設定する場合は、使っている WSL の版での挙動を確認する。反映には `wsl --shutdown` が必要。
[WSL の設定](https://learn.microsoft.com/windows/wsl/wsl-config)・[WSL の systemd](https://learn.microsoft.com/windows/wsl/systemd)
