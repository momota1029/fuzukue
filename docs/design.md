# 設計

設計はカードに移した（`cards/`）。入口は `cards/goal/workbench.md` と `cards/plan/roadmap.md`。

```sh
cargo run -q -p fuzukue-cards -- check
cargo run -q -p fuzukue-cards -- context roadmap      # 次の作業で読むべきカード
cargo run -q -p fuzukue-cards -- files crates/secretaire/src/claude.rs
```
