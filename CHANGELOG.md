# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 与 [SemVer](https://semver.org/lang/zh-CN/) 约定。

## [Unreleased]

### Added

- Tauri 2 + React + Vite 桌面应用骨架，取代原 Python/FastAPI 服务端 + 静态前端方案，整个应用单可执行文件分发，不再需要本地起 8000 端口
- AI 检索式生成新增「仅 OA（开放获取）期刊」开关（默认开启）：调用 LLM 时注入 OA 提示，并在 Rust 侧对生成的检索式 / 整个 review plan 强制追加 `AND OA=("All Open Access")`，避免命中付费墙触发出版商反爬导致账号封禁
- WoS 检索式校验器允许 `OA=` 字段标签
- GitHub Actions：`ci.yml`（pnpm build → cargo fmt --check → cargo check → cargo clippy -D warnings）与 `release.yml`（Windows / macOS Universal / Ubuntu 三平台矩阵 + Windows portable zip）
- 应用底部新增作者与仓库链接（通过 `tauri-plugin-shell` 跳转系统浏览器）
- 项目根 `CLAUDE.md` 列出推送前必跑的本地检查清单
- `dev.ps1` / `build.ps1` 一键启动 / 打包脚本

### Changed

- 把 `core/*` / `skills/*` / `schemas/*` / `main.py` 全部移植到 Rust：
  - 命令层：FastAPI 的 ~512 行 endpoint → 强类型 `#[tauri::command]`，SSE 流改为 Tauri event (`task-progress` / `task-done`)
  - 后台浏览器：自实现 raw CDP over WebSocket（`tokio-tungstenite`）取代 Playwright
  - 表格 IO：`calamine` + `rust_xlsxwriter` + `zip` + `csv` + `encoding_rs` 取代 pandas / openpyxl
  - HTML 解析：`scraper` crate 取代 BeautifulSoup
  - LLM 客户端：`reqwest` + stream 解析取代 `httpx`
- 任务下载从前端 `@tauri-apps/plugin-fs#writeFile` 改成 Rust 端 `std::fs::write` 命令（`task_export_meta` + `task_save_to`），避免 Tauri v2 `fs:default` ACL 拒绝写入

### Removed

- 删除 `core/*.py`、`skills/*.py`、`schemas/*.py`、`main.py`、`requirements.txt`、`start.bat`、`static/index.html`（已被 Rust + React 版本取代）
- 移除未再使用的 `@tauri-apps/plugin-fs` 依赖（前端 + Cargo + capability 三处一并清理）

### Fixed

- 修复"下载结果"按钮触发 `Command plugin:fs|write_file not allowed by ACL` 失败 —— 改走 Rust 命令直接落盘

### Notes

- Cargo workspace 根在 `src-tauri/`，所有 `cargo` 命令请在该目录下执行
- 推送前请按 `CLAUDE.md` 顺序在本地跑：`pnpm install --frozen-lockfile && pnpm build && cargo fmt --all && cargo check --all-targets --locked && cargo clippy --all-targets --locked -- -D warnings && cargo test`
