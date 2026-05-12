# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 与 [SemVer](https://semver.org/lang/zh-CN/) 约定。

## [0.2.0] - 2026-05-12

界面全面重做 + LLM 易用性增强。

### Added

- **shadcn / Tailwind 重做的界面**：HSL 双主题 token、glassy surface 卡片层、Radix Dialog / Switch / Label 等 11 个 UI 原语，全套 lucide-react 图标
- **左侧导航 + 主区**布局取代单列纵向堆叠：四个分区（LLM 配置 / AI 检索式 / 数据输入 / 处理流程），侧栏实时显示 LLM 就绪 / 数据会话 / 筛选状态徽章
- **暗色模式**：右上角太阳/月亮按钮一键切换，跟随系统 `prefers-color-scheme`，选择持久化到 localStorage
- **模型自动扫描**：Model 输入框右侧的刷新按钮调用 `scan_models` 命令 → GET `<base_url>/v1/models`，把结果灌进 HTML `<datalist>`，输入框现在支持打字补全 / 下拉选择
- **应用版本号**显示在左下角（通过 Vite `define` 从 `package.json` 注入），方便用户和 Release Notes 对应
- `release.yml` 现在自动从 `CHANGELOG.md` 抽取对应版本的段落作为 GitHub Release body，不再需要手动 `gh release edit`

### Changed

- 默认 Base URL 从 `https://api.openai.com/v1` 改为用户偏好的中转 `https://e-flowcode.cc/v1`
- WoS 检索式构造接受 `OA` 字段（用于 OA-only 模式）
- 几乎所有交互元素改用新 UI 原语；登录确认走 Radix Dialog，OA-only 开关改用 Switch

### Fixed

- 修复因 `pnpm-lock.yaml` 滞后导致 CI `--frozen-lockfile` 失败
- 修复 release workflow 中 Rust 1.95 新增 clippy lint（`while_let_loop` / `collapsible_match` / `sort_by_key`）在 CI 单独红的问题

## [0.1.0] - 2026-05-12

首个公开发布版本：Tauri 桌面应用全量重写。

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
