# wos-fetch — Web of Science 文献获取工具

一个面向本地使用场景的桌面应用（**Rust + Tauri 2 + React**），提供以下能力：

- 连接已登录的 Chromium 浏览器，通过 Web of Science 页面执行检索并抓取结果
- 上传 CSV / Excel / ZIP 格式的文献表
- 使用 LLM 对记录做相关性筛选
- 基于 DOI 批量获取文章全文，支持 HTTP 抓取和浏览器 fallback
- 导出结果为 Excel 或 ZIP 包

## 项目定位

这是一个本地运行的桌面应用，不是云端服务。所有数据保存在你自己的机器上。

当前设计假设：

- 运行环境为 Windows 10/11（macOS、Linux 应可编译但未做适配测试）
- 本机已安装 [Rust](https://rustup.rs/) 工具链 1.77+
- 本机已安装 [Node.js](https://nodejs.org/) 20+ 和 [pnpm](https://pnpm.io/)
- 本机可使用 Microsoft Edge 或 Google Chrome
- 使用者对 Web of Science 和目标出版商站点拥有合法访问权限

## 开发

```bash
# 一次性安装前端依赖
pnpm install

# 开发模式（前端热重载 + Rust 后端自动重新编译）
pnpm tauri dev

# 仅跑 Rust 单元测试
cd src-tauri && cargo test
```

## 打包发布

```bash
pnpm tauri build
```

生成的安装包在 `src-tauri/target/release/bundle/`（Windows 上是 `.msi` 和 `.exe`）。

## WoS 使用前提

若要使用 WoS 自动检索，需要提前打开已登录的 Chrome / Edge，并启用远程调试端口：

```bat
msedge.exe --remote-debugging-port=9222 --user-data-dir=%TEMP%\wos-debug
```

然后在该浏览器中打开并登录 Web of Science。

应用内也可以点击「启动浏览器」一键启动一个独立的调试 profile。

## LLM 配置

应用界面中需要填写：

- `API Base URL`（默认 `https://api.openai.com/v1`，支持任何 OpenAI 兼容接口）
- `API Key`
- `Model`

## 输入与输出

输入：

- WoS 检索结果（应用内自动抓取）
- 或手动上传 CSV / XLSX / XLS / ZIP

输出：

- `fetch_result.xlsx`（只有结果表时）
- `fetch_result.zip`（包含结果表 + `fulltext/*.md` 全文 + README.txt）

## 浏览器 Profile 位置

启动 WoS 自动检索时，工具会拉起一个**独立的 Chromium 浏览器实例**，并把它的用户数据保存在：

```text
%USERPROFILE%\wos-fetch-profile\
```

第一次在该 profile 内登录 WoS 后，后续运行会复用登录状态。

- **想换账号或彻底重置**：直接删除该文件夹即可
- **该文件夹包含 WoS 与（可能的）出版商网站 cookie**，请视为敏感数据，避免在多人共享的机器上裸放

## 项目结构

```text
wos-fetch/
├── src/                       # React 前端
│   ├── App.tsx                # 主组件（所有 UI 流程）
│   ├── main.tsx               # 入口
│   └── styles.css             # 样式
├── src-tauri/                 # Rust 后端
│   ├── src/
│   │   ├── main.rs            # 桌面入口
│   │   ├── lib.rs             # Tauri builder + command 注册
│   │   ├── commands.rs        # IPC 命令（前端通过 invoke 调用）
│   │   ├── error.rs           # 统一错误类型
│   │   ├── core/              # 通用工具（LLM 客户端、JSON 修复、表格 IO 等）
│   │   ├── schemas/           # 提示词模板常量
│   │   └── skills/            # 业务流程（查询构造、相关性筛选、全文获取、WoS 浏览器自动化）
│   ├── Cargo.toml             # Rust 依赖
│   └── tauri.conf.json        # Tauri 配置
├── package.json               # JS 依赖
└── vite.config.ts             # Vite 配置
```

## 已知限制

- 当前主要在 Windows 上测试
- WoS 自动化依赖浏览器页面结构，目标站点改版后可能需要调整 JS 注入
- 全文获取效果依赖 DOI 质量、站点可访问性、机构权限与反爬限制
- 任务状态保存在进程内存中，应用重启后会丢失

## 合规与使用边界

请仅在你对目标资源拥有合法访问权限的前提下使用本项目。

使用者应自行遵守：

- Web of Science 的服务条款
- 目标出版商网站的服务条款
- 所在学校、机构或单位的网络与资源使用政策
- 当地适用法律法规

本项目不提供账号，不绕过权限控制，也不保证对任何第三方站点的长期兼容性。

## 致谢

感谢 [@yaya200325](https://github.com/yaya200325) 协助编写了 wos-fetch 及配套综述写作工具最初版本的代码。

## 许可证

本项目采用 `Apache-2.0` 许可证，详见 [LICENSE](LICENSE)。
