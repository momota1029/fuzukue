### 作業役の実行環境

- 作業中の発言、受付への質問、最終報告は英語で書く。成果物は用途と既存の文体に合う言語で書き、報告に含めるパス、コード、ログ、引用は原文を保つ。
- `/tmp` が存在しないか、書き込めないことがある。一時ファイルにはジョブ専用の `"$TMPDIR"` を使う。例は `mktemp -d "${TMPDIR:-/tmp}/fuzukue-job.XXXXXXXX"`。`TMP` と `TEMP` も同じ場所を指す。
- `scripts/shot.sh` はループバック TCP と Xvfb の Unix ソケットを使う。`AF_INET: Operation not permitted` や Xvfb の bind 拒否で撮影できない環境では、終了コード `77` を撮影スキップとして報告する。
- `scripts/shot.sh` が終了コード `77` を返したら、`list_recipes` に撮影用レシピがある場合は `run_recipe` で依頼し、なければ `capture_screenshot` を使って画像を確認する。
- `.git` が読み取り専用で `git commit` が失敗したら、変更を作業ツリーに残してその旨を伝える。コミットは受付が行う。
- クライアントを直接ビルドするときは、書き込める `TMPDIR` と `RUST_FONTCONFIG_DLOPEN=1` を使う。必要ならリポジトリ内に一時ディレクトリを作って `TMPDIR` に指定する。
- `xcb`、`xkbcommon`、`xkbcommon-x11` のリンク名がなければ、`scripts/shot.sh` と同様に一時ディレクトリ内で `libxcb.so.1` → `libxcb.so`、`libxkbcommon.so.0` → `libxkbcommon.so`、`libxkbcommon-x11.so.0` → `libxkbcommon-x11.so` の別名を作る。そのディレクトリを `LIBRARY_PATH="$link_dir${LIBRARY_PATH:+:$LIBRARY_PATH}"` で渡す。
- サーバーからサービスモードや自己更新用の環境変数が引き継がれることがある。secretaire のテストは `env -u FUZUKUE_SERVICE -u FUZUKUE_SELF_UPDATE_READY -u FUZUKUE_SELF_UPDATE_SOURCE cargo test -q -p secretaire` で実行する。
- fuzukue のビルドとテストは `bash scripts/worker-cargo.sh test -q -p secretaire` のように実行する。リンクした worktree 間で主チェックアウトの `target/worker-shared` を共有する。先行ビルドがあれば待ち、テストはクレートやテスト名で絞る。
- 並行ビルド中の一時的なリンクエラーは、先行ジョブの終了後に同じコマンドで再確認する。
- 実時間を比較するテストは高負荷時に不安定になることがある。該当テストを単独で再実行し、最初の結果と再実行の結果を残す。

<!-- English -->

### Worker environment

- Write progress messages, questions for the front desk, and final reports in English. Follow the language and style of each deliverable, and preserve paths, code, logs, and quotations as written.
- `/tmp` may be missing or unwritable. Use the job's `"$TMPDIR"` for temporary files, for example `mktemp -d "${TMPDIR:-/tmp}/fuzukue-job.XXXXXXXX"`. `TMP` and `TEMP` point to the same directory.
- `scripts/shot.sh` needs loopback TCP and Xvfb's Unix sockets. If `AF_INET: Operation not permitted` or an Xvfb bind denial prevents capture, report exit status `77` as a skipped capture.
- If `scripts/shot.sh` exits 77, request the capture with `run_recipe` when `list_recipes` shows a capture recipe. Otherwise use `capture_screenshot`. Check the resulting images.
- If `git commit` fails because `.git` is read-only, leave the changes in the working tree and say so. The front desk commits.
- For direct client builds, use a writable `TMPDIR` and `RUST_FONTCONFIG_DLOPEN=1`. If needed, create a temporary directory inside the repository and set `TMPDIR` to it.
- If linker names for `xcb`, `xkbcommon`, and `xkbcommon-x11` are missing, follow `scripts/shot.sh`: create temporary aliases from `libxcb.so.1` to `libxcb.so`, `libxkbcommon.so.0` to `libxkbcommon.so`, and `libxkbcommon-x11.so.0` to `libxkbcommon-x11.so`. Pass the directory via `LIBRARY_PATH="$link_dir${LIBRARY_PATH:+:$LIBRARY_PATH}"`.
- The worker can inherit service mode and self-update variables from the server. Run secretaire tests with `env -u FUZUKUE_SERVICE -u FUZUKUE_SELF_UPDATE_READY -u FUZUKUE_SELF_UPDATE_SOURCE cargo test -q -p secretaire`.
- Build and test fuzukue with `bash scripts/worker-cargo.sh test -q -p secretaire`. Linked worktrees share `target/worker-shared` in the primary checkout. Wait for earlier builds and narrow tests by crate or test name where possible.
- For transient linker errors during concurrent builds, retry the same command once the other build finishes.
- Tests that compare wall-clock times can flake under load. Rerun the affected test in isolation and record both results.
