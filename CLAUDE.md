# wos-fetch 项目约定

## 推送前必跑的检查

CI（`.github/workflows/ci.yml`）会跑这四样：`pnpm build` → `cargo fmt --check` → `cargo check --all-targets --locked` → `cargo clippy --all-targets --locked -- -D warnings`。任一失败就红。**`git push` 之前必须本地复跑同一组，确认全绿再推**，否则等于把 CI 当 linter，浪费一轮 runner 时间。

最短复跑：

```bash
# 前端
pnpm install --frozen-lockfile
pnpm build

# 后端（Cargo.toml 在 src-tauri/）
cd src-tauri
cargo fmt --all
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test
```

注意：

- `cargo fmt --all` 是直接改文件，不是 `--check`；提交前应**保证 fmt 已落地**而不是仅校验。
- 任何动 `package.json` 依赖的改动，**必须紧跟 `pnpm install`** 并把 `pnpm-lock.yaml` 一并提交，否则 CI 在 `--frozen-lockfile` 那一步就挂。
- 本机 pnpm 版本（`pnpm -v`）必须和 workflow 里 `pnpm/action-setup` 的 `version` 对齐，否则 lockfile 兼容性可能踩坑。当前两端都钉 pnpm 10。

## 环境

- Rust 工程根在 `src-tauri/`，不是仓库根。所有 `cargo` 命令都要在 `src-tauri/` 下跑（或用 `cargo -C src-tauri ...`）。
- 前端用 pnpm，不要混用 npm/yarn。
- Tauri v2 的 `fs:default` 是只读权限集；要写盘的话走 Rust 端 `std::fs::write` 的 command，不要给前端加 `fs:allow-write-file` —— 那是放大攻击面。

## 设计约束（不要忘）

- 「仅 OA 期刊」默认开启：目标用户多为写课程论文的学生，限定 OA 可避免出版商反爬触发账号/IP 封禁。详见 [memory/project-oa-only-mode]。
