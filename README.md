# wos-fetch — Web of Science 文献获取工具

一个面向本地使用场景的文献获取辅助工具，提供以下能力：

- 连接已登录的 Chromium 浏览器，通过 Web of Science 页面执行检索并抓取结果
- 上传 CSV / Excel / ZIP 格式的文献表
- 使用 LLM 对记录做相关性筛选
- 基于 DOI 批量获取文章全文，支持 HTTP 获取和浏览器 fallback
- 导出结果为 Excel 或 ZIP 包

## 项目定位

这是一个本地运行的 Windows 工具，不是云端服务。

当前设计假设：

- 运行环境为 Windows
- 本机已安装 Python 3.10+
- 本机可使用 Microsoft Edge 或 Google Chrome
- 使用者对 Web of Science 和目标出版商站点拥有合法访问权限

## 功能概览

1. WoS 自动检索
   - 连接本机已打开并登录的 Chrome / Edge 调试端口
   - 提交 WoS Advanced Search 检索式
   - 抓取检索结果页中的记录

2. 文件上传
   - 支持 CSV、Excel、ZIP
   - 可读取已有中间结果继续处理

3. 相关性筛选
   - 调用兼容 OpenAI API 的模型接口
   - 对记录批量评分并筛选

4. 全文获取
   - 优先通过 HTTP 抓取全文页面
   - 必要时回退到浏览器页面抓取
   - 适合处理机构网络已登录的出版商站点

5. 结果导出
   - Excel 结果表
   - ZIP 结果包（包含结果表和拆分出的全文 Markdown 文件）

## 安装

```bash
pip install -r requirements.txt
```

## 启动

### 方式 1：直接启动

```bash
uvicorn main:app --host 127.0.0.1 --port 8001 --reload
```

浏览器打开：

```text
http://127.0.0.1:8001
```

### 方式 2：Windows 批处理启动

```bat
start.bat
```

## WoS 使用前提

若要使用 WoS 自动检索，需要提前打开已登录的 Chrome / Edge，并启用远程调试端口。

例如：

```bat
msedge.exe --remote-debugging-port=9222 --user-data-dir=%TEMP%\wos-debug
```

然后在该浏览器中打开并登录 Web of Science。

## LLM 配置

界面中需要填写：

- `API Base URL`
- `API Key`
- `Model`

默认 Base URL 为：

```text
https://api.openai.com/v1
```

只要接口兼容 OpenAI 风格请求，也可以替换为其他服务地址。

## 浏览器 Profile 位置

启动 WoS 自动检索时，工具会拉起一个**独立的 Chromium 浏览器实例**（与你日常使用的浏览器隔离），并把它的用户数据保存在：

```text
%USERPROFILE%\wos-fetch-profile\
```

第一次在该 profile 内登录 WoS 后，后续运行会复用登录状态，无需重复登录。

- **想换账号或彻底重置**：直接删除该文件夹即可
- **该文件夹包含 WoS 与（可能的）出版商网站 cookie**，请视为敏感数据，避免在多人共享的机器上裸放

## 输入与输出

输入：

- WoS 检索结果
- CSV / XLSX / XLS / ZIP

输出：

- `fetch_result.xlsx`
- 或 `fetch_result.zip`

ZIP 包中通常包含：

- 结果表
- `fulltext/*.md`
- 说明文件

## 已知限制

- 当前主要面向 Windows 环境
- WoS 自动化依赖浏览器页面结构，目标站点改版后可能需要调整
- 全文获取效果依赖 DOI 质量、站点可访问性、机构权限与反爬限制
- 未内置持久化数据库，任务状态保存在进程内存中，服务重启后会丢失

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
