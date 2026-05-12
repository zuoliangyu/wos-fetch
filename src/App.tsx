import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";

// ----------------------------------------------------------------------------
// Types matching the Rust IPC contract
// ----------------------------------------------------------------------------

type StatusType = "info" | "ok" | "err";

interface Status {
  msg: string;
  type: StatusType;
  spinner?: boolean;
}

interface UploadResponse {
  session_id: string;
  row_count: number;
  doi_count: number;
  columns: string[];
}

interface TaskRecord {
  task_id: string;
  status: "running" | "done" | "error";
  error: string;
  result_format: string;
  wos_session_id: string;
  screened_session_id: string;
  row_count: number;
  added_count: number;
  source_row_count: number;
}

interface PublisherSummary {
  name: string;
  url: string;
  count: number;
}

interface PublishersResponse {
  publishers: PublisherSummary[];
}

interface DebugTarget {
  id: string;
  type: string;
  url: string;
  title: string;
  webSocketDebuggerUrl: string;
}

interface DirectionItem {
  direction_index: string;
  direction_name: string;
  purpose: string;
  suggested_section: string;
  search_query: string;
  expected_records: string;
  include_hint: string;
  exclude_hint: string;
  run_status?: "pending" | "running" | "ok" | "err";
  row_count?: number;
  added_count?: number;
}

interface Plan {
  normalized_topic: string;
  inferred_domain: string;
  review_objective: string;
  search_directions: DirectionItem[];
}

interface ProgressEvent {
  task_id: string;
  message: string;
  timestamp: number;
}

interface TaskDoneEvent {
  task_id: string;
  status: "done" | "error";
  error: string;
}

interface TaskExportMeta {
  filename: string;
  ext: string;
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

const PUBLISHER_RISK_HIGH = new Set([
  "Elsevier (ScienceDirect)", "Wiley Online Library", "Springer",
  "Taylor & Francis", "SAGE Journals", "Wolters Kluwer",
]);
const PUBLISHER_RISK_MEDIUM = new Set([
  "ACS Publications", "Royal Society of Chemistry", "IEEE Xplore",
  "ACM Digital Library", "Oxford University Press", "Cambridge University Press",
  "Nature Portfolio", "American Physical Society", "American Physiological Society",
  "American Society for Microbiology", "BMJ", "JAMA Network",
  "New England Journal of Medicine", "Science (AAAS)",
]);

function publisherRiskClass(name: string): string {
  if (PUBLISHER_RISK_HIGH.has(name)) return "risk-badge risk-high";
  if (PUBLISHER_RISK_MEDIUM.has(name)) return "risk-badge risk-medium";
  return "";
}

function publisherRiskLabel(name: string): string {
  if (PUBLISHER_RISK_HIGH.has(name)) return "高风险";
  if (PUBLISHER_RISK_MEDIUM.has(name)) return "中风险";
  return "";
}

// Wait for a task to complete by listening for the "task-done" event.
// Returns the final TaskRecord on success.
async function waitForTask(taskId: string): Promise<TaskRecord> {
  return new Promise((resolve, reject) => {
    let unlisten: (() => void) | null = null;
    listen<TaskDoneEvent>("task-done", async (event) => {
      if (event.payload.task_id !== taskId) return;
      unlisten?.();
      if (event.payload.status === "error") {
        reject(new Error(event.payload.error || "任务失败"));
        return;
      }
      try {
        const record = await invoke<TaskRecord>("task_status", { taskId });
        resolve(record);
      } catch (err) {
        reject(err);
      }
    }).then((u) => {
      unlisten = u;
    });
  });
}

// ----------------------------------------------------------------------------
// App component
// ----------------------------------------------------------------------------

export default function App() {
  // LLM config
  const [model, setModel] = useState("gpt-4o-mini");
  const [baseUrl, setBaseUrl] = useState("https://api.openai.com/v1");
  const [apiKey, setApiKey] = useState("");
  const [timeoutSec, setTimeoutSec] = useState(120);

  // Card collapse states
  const [cfgOpen, setCfgOpen] = useState(true);
  const [queryOpen, setQueryOpen] = useState(true);

  // AI query/plan generation
  const [topic, setTopic] = useState("");
  const [directionCount, setDirectionCount] = useState("auto");
  const [oaOnly, setOaOnly] = useState(true);
  const [generatedQuery, setGeneratedQuery] = useState("");
  const [singleQueryVisible, setSingleQueryVisible] = useState(false);
  const [plan, setPlan] = useState<Plan | null>(null);
  const [queryGenStatus, setQueryGenStatus] = useState<Status | null>(null);
  const [genQueryBusy, setGenQueryBusy] = useState(false);
  const [genPlanBusy, setGenPlanBusy] = useState(false);
  const [planSearchRunning, setPlanSearchRunning] = useState(false);

  // WoS / upload
  const [tab, setTab] = useState<"wos" | "upload">("wos");
  const [wosQuery, setWosQuery] = useState("");
  const [debugPort, setDebugPort] = useState(9222);
  const [maxPages, setMaxPages] = useState(5);
  const [pageWait, setPageWait] = useState(20);
  const [wosStatus, setWosStatus] = useState<Status | null>(null);
  const [wosBusy, setWosBusy] = useState(false);
  const [uploadStatus, setUploadStatus] = useState<Status | null>(null);
  const [uploadBusy, setUploadBusy] = useState(false);
  const uploadInputRef = useRef<HTMLInputElement>(null);

  // Session state
  const [currentSessionId, setCurrentSessionId] = useState<string | null>(null);
  const [sessionLabel, setSessionLabel] = useState("");
  const [screenedSessionId, setScreenedSessionId] = useState<string | null>(null);
  const [screenedLabel, setScreenedLabel] = useState("");

  // Step 1: screening
  const [batchSize, setBatchSize] = useState(12);
  const [step1Status, setStep1Status] = useState<Status | null>(null);
  const [log1, setLog1] = useState<string[]>([]);
  const [screeningBusy, setScreeningBusy] = useState(false);
  const [currentScreeningTask, setCurrentScreeningTask] = useState<string | null>(null);

  // Step 2: fulltext
  const [fulltextWorkers, setFulltextWorkers] = useState(2);
  const [browserWait, setBrowserWait] = useState(25);
  const [useBrowser, setUseBrowser] = useState(false);
  const [step2Status, setStep2Status] = useState<Status | null>(null);
  const [log2, setLog2] = useState<string[]>([]);
  const [fulltextBusy, setFulltextBusy] = useState(false);
  const [currentFulltextTask, setCurrentFulltextTask] = useState<string | null>(null);
  const [downloadable, setDownloadable] = useState<string | null>(null);

  // Login modal
  const [modalPublishers, setModalPublishers] = useState<PublisherSummary[]>([]);
  const [modalOpen, setModalOpen] = useState(false);

  const llmConfigArg = useMemo(() => ({
    model: model.trim(),
    base_url: baseUrl.trim() || "https://api.openai.com/v1",
    api_key: apiKey.trim(),
    timeout_seconds: timeoutSec,
  }), [model, baseUrl, apiKey, timeoutSec]);

  const llmConfigured = llmConfigArg.model && llmConfigArg.api_key;

  // -- Progress event subscription --
  useEffect(() => {
    const unlistens: (() => void)[] = [];
    listen<ProgressEvent>("task-progress", (event) => {
      const { task_id, message } = event.payload;
      if (task_id === currentScreeningTask) {
        setLog1((l) => [...l, message]);
        setStep1Status({ msg: message, type: "info" });
      } else if (task_id === currentFulltextTask) {
        setLog2((l) => [...l, message]);
        setStep2Status({ msg: message, type: "info" });
      }
    }).then((u) => unlistens.push(u));
    return () => { unlistens.forEach((u) => u()); };
  }, [currentScreeningTask, currentFulltextTask]);

  // -- Session helpers --
  const acceptSession = useCallback((id: string, label: string) => {
    setCurrentSessionId(id);
    setSessionLabel(label);
    setScreenedSessionId(null);
    setScreenedLabel("");
    setDownloadable(null);
  }, []);

  const skipScreening = () => {
    if (!currentSessionId) { alert("请先完成数据输入"); return; }
    setScreenedSessionId(currentSessionId);
    setScreenedLabel("已跳过筛选，使用全部记录");
    setStep1Status({ msg: "已跳过筛选，可直接在第二步获取全文", type: "ok" });
  };

  // -- AI query generation --
  const generateQuery = async () => {
    if (!topic.trim()) { alert("请先填写研究主题"); return; }
    if (!llmConfigured) { alert("请先配置 API Key 和 Model"); return; }
    setGenQueryBusy(true);
    setSingleQueryVisible(false);
    setQueryGenStatus({ msg: "正在生成检索式...", type: "info", spinner: true });
    try {
      const query = await invoke<string>("generate_query", {
        args: { topic: topic.trim(), llm: llmConfigArg, oa_only: oaOnly },
      });
      setGeneratedQuery(query);
      setSingleQueryVisible(true);
      setQueryGenStatus({ msg: "检索式生成成功，可编辑后点击「填入检索框」", type: "ok" });
    } catch (err: any) {
      setQueryGenStatus({ msg: String(err?.message || err), type: "err" });
    } finally {
      setGenQueryBusy(false);
    }
  };

  const generatePlan = async () => {
    if (!topic.trim()) { alert("请先填写研究主题"); return; }
    if (!llmConfigured) { alert("请先配置 API Key 和 Model"); return; }
    setGenPlanBusy(true);
    setPlan(null);
    setQueryGenStatus({ msg: "正在生成完整检索规划（可能需要 30-60 秒）...", type: "info", spinner: true });
    try {
      const data = await invoke<any>("generate_plan", {
        args: { topic: topic.trim(), llm: { ...llmConfigArg, timeout_seconds: Math.max(180, timeoutSec) }, direction_count: directionCount, oa_only: oaOnly },
      });
      const dirs: DirectionItem[] = (data.search_directions || []).map((d: any) => ({ ...d, run_status: "pending", row_count: 0, added_count: 0 }));
      setPlan({ ...data, search_directions: dirs });
      setQueryGenStatus({ msg: `规划生成成功，共 ${dirs.length} 个检索方向`, type: "ok" });
    } catch (err: any) {
      setQueryGenStatus({ msg: String(err?.message || err), type: "err" });
    } finally {
      setGenPlanBusy(false);
    }
  };

  const fillQuery = (query: string) => {
    setWosQuery(query);
    setTab("wos");
    setWosStatus({ msg: "检索式已填入，可直接点击「搜索并抓取」", type: "ok" });
  };

  // -- WoS search (single & plan batch) --
  const startWosSearchTask = async (query: string, appendSessionId: string): Promise<string> => {
    return invoke<string>("wos_search", {
      args: {
        query,
        debug_port: debugPort,
        max_pages: maxPages,
        page_wait_seconds: pageWait,
        append_session_id: appendSessionId,
      },
    });
  };

  const doWosSearch = async () => {
    const query = wosQuery.trim();
    if (!query) { alert("请填写 WoS 检索式"); return; }
    setWosBusy(true);
    setWosStatus({ msg: "提交检索式...", type: "info", spinner: true });
    try {
      const taskId = await startWosSearchTask(query, "");
      const task = await waitForTask(taskId);
      if (task.wos_session_id) {
        acceptSession(task.wos_session_id, `WoS ${task.row_count} 条`);
        setWosStatus({ msg: `已抓取 ${task.row_count} 条记录，请继续执行第一步筛选`, type: "ok" });
      }
    } catch (err: any) {
      setWosStatus({ msg: String(err?.message || err), type: "err" });
    } finally {
      setWosBusy(false);
    }
  };

  const runPlanSearchAll = async () => {
    if (!plan || planSearchRunning) return;
    setPlanSearchRunning(true);
    setTab("wos");
    let mergedSessionId = "";
    const directions = plan.search_directions.slice();
    try {
      for (let i = 0; i < directions.length; i++) {
        const item = directions[i];
        if (!item.search_query?.trim()) {
          directions[i] = { ...item, run_status: "err" };
          setPlan({ ...plan, search_directions: directions });
          throw new Error(`第 ${i + 1} 条检索方向缺少检索式`);
        }
        setWosQuery(item.search_query);
        directions[i] = { ...item, run_status: "running" };
        setPlan({ ...plan, search_directions: directions });
        setWosStatus({ msg: `正在执行第 ${i + 1}/${directions.length} 条检索方向...`, type: "info", spinner: true });
        const taskId = await startWosSearchTask(item.search_query, mergedSessionId);
        const task = await waitForTask(taskId);
        mergedSessionId = task.wos_session_id || mergedSessionId;
        directions[i] = {
          ...item,
          run_status: "ok",
          row_count: task.row_count,
          added_count: task.added_count || task.row_count,
        };
        setPlan({ ...plan, search_directions: directions });
      }
      if (mergedSessionId) {
        const total = directions.reduce((sum, item) => sum + (item.added_count || 0), 0);
        acceptSession(mergedSessionId, `WoS 合并 ${total} 条`);
        setWosStatus({ msg: "全部检索完成，已合并为同一批结果，可继续执行第一步筛选", type: "ok" });
      }
    } catch (err: any) {
      setWosStatus({ msg: String(err?.message || err), type: "err" });
    } finally {
      setPlanSearchRunning(false);
    }
  };

  const checkTargets = async () => {
    try {
      const targets = await invoke<DebugTarget[]>("wos_targets", { body: { debug_port: debugPort } });
      if (targets.length > 0) {
        const preview = targets.slice(0, 3).map((t) => t.title || t.url).join(" | ");
        setWosStatus({ msg: `检测到 ${targets.length} 个标签：${preview}`, type: "ok" });
      } else {
        setWosStatus({ msg: `未检测到浏览器（端口 ${debugPort}），请先启动浏览器`, type: "err" });
      }
    } catch (err: any) {
      setWosStatus({ msg: String(err?.message || err), type: "err" });
    }
  };

  const launchBrowser = async () => {
    setWosStatus({ msg: "正在启动浏览器...", type: "info", spinner: true });
    try {
      const taskId = await invoke<string>("wos_launch", { body: { debug_port: debugPort } });
      await waitForTask(taskId);
      setWosStatus({ msg: "浏览器已就绪，请登录 WoS 后再搜索", type: "ok" });
    } catch (err: any) {
      setWosStatus({ msg: String(err?.message || err), type: "err" });
    }
  };

  // -- Upload --
  const doUpload = async () => {
    const fi = uploadInputRef.current;
    if (!fi?.files?.length) { alert("请选择文件"); return; }
    const file = fi.files[0];
    setUploadBusy(true);
    setUploadStatus({ msg: "上传中...", type: "info", spinner: true });
    try {
      const buffer = await file.arrayBuffer();
      const bytes = Array.from(new Uint8Array(buffer));
      const resp = await invoke<UploadResponse>("upload_file", {
        filename: file.name,
        bytes,
      });
      acceptSession(resp.session_id, `文件 ${resp.row_count} 行`);
      setUploadStatus({
        msg: `上传成功：${resp.row_count} 行，DOI ${resp.doi_count} 条，请继续执行第一步筛选`,
        type: "ok",
      });
    } catch (err: any) {
      setUploadStatus({ msg: String(err?.message || err), type: "err" });
    } finally {
      setUploadBusy(false);
    }
  };

  // -- Step 1: screening --
  const runScreening = async () => {
    if (!currentSessionId) { alert("请先完成数据输入"); return; }
    setScreeningBusy(true);
    setLog1([]);
    setScreenedLabel("");
    setScreenedSessionId(null);
    setStep1Status({ msg: "正在启动相关性筛选...", type: "info", spinner: true });
    try {
      const taskId = await invoke<string>("run_screening", {
        args: {
          session_id: currentSessionId,
          llm: llmConfigArg,
          topic: topic.trim(),
          batch_size: batchSize,
        },
      });
      setCurrentScreeningTask(taskId);
      const task = await waitForTask(taskId);
      if (task.screened_session_id) {
        setScreenedSessionId(task.screened_session_id);
        setScreenedLabel(`筛选后剩余 ${task.row_count} 条`);
        setStep1Status({ msg: "筛选完成，请继续执行第二步获取全文", type: "ok" });
      }
    } catch (err: any) {
      setStep1Status({ msg: String(err?.message || err), type: "err" });
      setLog1((l) => [...l, `[ERROR] ${err?.message || err}`]);
    } finally {
      setScreeningBusy(false);
      setCurrentScreeningTask(null);
    }
  };

  // -- Step 2: fulltext --
  const startFetch = async () => {
    setDownloadable(null);
    setLog2([]);
    setStep2Status({ msg: "正在启动全文获取...", type: "info", spinner: true });
    try {
      const taskId = await invoke<string>("run_fulltext", {
        args: {
          session_id: screenedSessionId,
          timeout_seconds: timeoutSec,
          debug_port: debugPort,
          use_browser: useBrowser,
          browser_wait_seconds: browserWait,
          workers: fulltextWorkers,
        },
      });
      setCurrentFulltextTask(taskId);
      const task = await waitForTask(taskId);
      setDownloadable(task.task_id);
      setStep2Status({ msg: "全文获取完成，可下载结果", type: "ok" });
    } catch (err: any) {
      setStep2Status({ msg: String(err?.message || err), type: "err" });
      setLog2((l) => [...l, `[ERROR] ${err?.message || err}`]);
    } finally {
      setFulltextBusy(false);
      setCurrentFulltextTask(null);
    }
  };

  const runFulltext = async () => {
    if (!screenedSessionId) { alert("请先完成相关性筛选"); return; }
    setFulltextBusy(true);
    setStep2Status({ msg: "正在分析所需出版商...", type: "info", spinner: true });
    try {
      const resp = await invoke<PublishersResponse>("run_fulltext_publishers", {
        sessionId: screenedSessionId,
      });
      if (resp.publishers.length > 0) {
        setModalPublishers(resp.publishers);
        setModalOpen(true);
        return;
      }
    } catch {
      /* fall through to direct fetch */
    }
    await startFetch();
  };

  const proceedFetch = () => {
    setModalOpen(false);
    void startFetch();
  };

  const closeModal = () => {
    setModalOpen(false);
    setFulltextBusy(false);
    setStep2Status(null);
  };

  const openInBrowser = async (url: string) => {
    try {
      const resp = await invoke<{ ok: boolean; target_id: string; error: string }>("browser_open", {
        args: { url, debug_port: debugPort },
      });
      if (!resp.ok) alert("无法在调试浏览器中打开：" + (resp.error || "未知错误"));
    } catch (err: any) {
      alert("请求失败：" + String(err?.message || err));
    }
  };

  const downloadResult = async () => {
    if (!downloadable) return;
    try {
      const meta = await invoke<TaskExportMeta>("task_export_meta", { taskId: downloadable });
      const dest = await save({
        defaultPath: meta.filename,
        filters: [{ name: meta.ext === "zip" ? "ZIP" : "Excel", extensions: [meta.ext] }],
      });
      if (!dest) return;
      await invoke("task_save_to", { taskId: downloadable, dest });
    } catch (err: any) {
      alert("保存失败：" + String(err?.message || err));
    }
  };

  // -- Render --
  const modalHasHighRisk = modalPublishers.some((p) => PUBLISHER_RISK_HIGH.has(p.name));

  return (
    <div className="app">
      <header>wos-fetch &mdash; 文献获取工具</header>
      <div className="container">

        {/* LLM 配置 */}
        <div className="card">
          <div className="card-header collapsible" onClick={() => setCfgOpen(!cfgOpen)}>
            LLM 配置 <span>{cfgOpen ? "▾" : "▸"}</span>
          </div>
          {cfgOpen && (
            <div className="card-body">
              <div className="row">
                <div className="fgroup">
                  <label>API Base URL</label>
                  <input type="text" value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
                </div>
                <div className="fgroup">
                  <label>API Key</label>
                  <input type="password" value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="sk-..." />
                </div>
              </div>
              <div className="row">
                <div className="fgroup">
                  <label>Model</label>
                  <input type="text" value={model} onChange={(e) => setModel(e.target.value)} />
                </div>
                <div className="fgroup">
                  <label>Timeout（秒）</label>
                  <input type="number" value={timeoutSec} min={30} max={600} onChange={(e) => setTimeoutSec(+e.target.value)} />
                </div>
              </div>
            </div>
          )}
        </div>

        {/* AI 检索式生成 */}
        <div className="card">
          <div className="card-header collapsible" onClick={() => setQueryOpen(!queryOpen)}>
            AI 检索式生成（可选） <span>{queryOpen ? "▾" : "▸"}</span>
          </div>
          {queryOpen && (
            <div className="card-body">
              <div className="fgroup">
                <label>研究主题描述（中文，详细描述研究范围、关键词、时间范围、排除条件等）</label>
                <textarea value={topic} onChange={(e) => setTopic(e.target.value)} rows={4}
                  placeholder="例如：近五年基于深度学习的医学影像分割方法综述..." />
              </div>
              <div className="fgroup" style={{ marginBottom: 10 }}>
                <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer", fontWeight: 500 }}>
                  <input type="checkbox" checked={oaOnly} onChange={(e) => setOaOnly(e.target.checked)} />
                  仅 OA（开放获取）期刊 — 推荐用于课程论文
                </label>
                <div style={{ fontSize: 12, color: oaOnly ? "#666" : "#b54708", marginTop: 4, paddingLeft: 22 }}>
                  {oaOnly
                    ? "已限定 OA 期刊：抓取仅命中开放获取文献，可避免触发 Elsevier / Wiley / Springer 等出版商的反爬机制。"
                    : "⚠ 未限定 OA：可能命中付费墙文献，频繁抓取容易导致 WoS 账号或 IP 被出版商封禁，请确认你的使用场景。"}
                </div>
              </div>
              <div className="row" style={{ marginBottom: 10 }}>
                <div className="fgroup">
                  <label>检索方向数量（生成完整规划时）</label>
                  <select value={directionCount} onChange={(e) => setDirectionCount(e.target.value)}>
                    <option value="auto">自动（根据主题宽窄）</option>
                    <option value="3">3 条</option>
                    <option value="5">5 条</option>
                    <option value="7">7 条</option>
                    <option value="10">10 条</option>
                  </select>
                </div>
                <div style={{ paddingTop: 18, display: "flex", gap: 8, alignItems: "flex-start" }}>
                  <button className="btn-secondary btn-sm" onClick={generateQuery} disabled={genQueryBusy}>
                    生成单条检索式
                  </button>
                  <button className="btn-purple btn-sm" onClick={generatePlan} disabled={genPlanBusy}>
                    生成完整检索规划
                  </button>
                </div>
              </div>
              {queryGenStatus && (
                <div className={`status-bar status-${queryGenStatus.type}`}>
                  {queryGenStatus.spinner && <span className="spinner" />}
                  {queryGenStatus.msg}
                </div>
              )}

              {singleQueryVisible && (
                <>
                  <div className="fgroup">
                    <label>生成的检索式（可直接编辑）</label>
                    <textarea value={generatedQuery} onChange={(e) => setGeneratedQuery(e.target.value)} rows={3} />
                  </div>
                  <button className="btn-sm btn-primary" onClick={() => fillQuery(generatedQuery.trim())}>填入检索框</button>
                </>
              )}

              {plan && (
                <div>
                  <hr />
                  <div style={{ fontWeight: 600, color: "#444", marginBottom: 8, fontSize: 13 }}>
                    检索规划结果（{plan.search_directions.length} 个方向）
                  </div>
                  <div style={{ fontSize: 12, color: "#555", marginBottom: 10, lineHeight: 1.6 }}>
                    {[
                      plan.normalized_topic && `主题：${plan.normalized_topic}`,
                      plan.inferred_domain && `领域：${plan.inferred_domain}`,
                      plan.review_objective && `目标：${plan.review_objective}`,
                    ].filter(Boolean).join("　|　")}
                  </div>
                  <div style={{ display: "flex", gap: 8, marginBottom: 10 }}>
                    <button className="btn-primary btn-sm" onClick={runPlanSearchAll}
                      disabled={planSearchRunning || !plan.search_directions.length}>
                      全部检索
                    </button>
                  </div>
                  {plan.search_directions.map((d, i) => (
                    <div className="dir-card" key={i}>
                      <div className="dir-card-head">
                        <div className="dir-card-title">{d.direction_index || i + 1}. {d.direction_name}</div>
                        <div className={`dir-status dir-status-${d.run_status || "pending"}`}>
                          {d.run_status === "running" ? "检索中" :
                           d.run_status === "ok" ? `完成 +${d.added_count || d.row_count || 0}` :
                           d.run_status === "err" ? "失败" : "待执行"}
                        </div>
                      </div>
                      {d.purpose && <div className="dir-card-meta">{d.purpose}</div>}
                      <div className="dir-card-query" onClick={() => fillQuery(d.search_query)}>{d.search_query}</div>
                      <div className="dir-card-meta">
                        {d.suggested_section && `章节：${d.suggested_section}　`}
                        {d.expected_records && `预期记录数：${d.expected_records}`}
                      </div>
                    </div>
                  ))}
                  <div className="hint">点击检索式可将其填入下方 WoS 检索框</div>
                </div>
              )}
            </div>
          )}
        </div>

        {/* 输入来源 */}
        <div className="card">
          <div className="card-header">输入来源</div>
          <div className="card-body">
            <div className="tabs">
              <div className={`tab ${tab === "wos" ? "active" : ""}`} onClick={() => setTab("wos")}>WoS 自动搜索</div>
              <div className={`tab ${tab === "upload" ? "active" : ""}`} onClick={() => setTab("upload")}>上传文件</div>
            </div>

            {tab === "wos" && (
              <div>
                <div className="row">
                  <div className="fgroup">
                    <label>WoS 检索式（Advanced Search 语法）</label>
                    <textarea value={wosQuery} onChange={(e) => setWosQuery(e.target.value)} rows={3}
                      placeholder="TS=(machine learning AND review) AND PY=(2019-2024)" />
                  </div>
                  <div>
                    <div className="fgroup">
                      <label>浏览器调试端口</label>
                      <input type="number" value={debugPort} min={1024} max={65535} onChange={(e) => setDebugPort(+e.target.value)} />
                    </div>
                    <div className="fgroup">
                      <label>最多抓取页数</label>
                      <input type="number" value={maxPages} min={1} max={50} onChange={(e) => setMaxPages(+e.target.value)} />
                    </div>
                    <div className="fgroup">
                      <label>每页最长等待（秒）</label>
                      <input type="number" value={pageWait} min={5} max={120} onChange={(e) => setPageWait(+e.target.value)} />
                    </div>
                  </div>
                </div>
                <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                  <button className="btn-secondary btn-sm" onClick={checkTargets}>检测浏览器</button>
                  <button className="btn-secondary btn-sm" onClick={launchBrowser}>启动浏览器</button>
                  <button className="btn-primary" onClick={doWosSearch} disabled={wosBusy}>搜索并抓取</button>
                </div>
                {wosStatus && (
                  <div className={`status-bar status-${wosStatus.type}`}>
                    {wosStatus.spinner && <span className="spinner" />}
                    {wosStatus.msg}
                  </div>
                )}
              </div>
            )}

            {tab === "upload" && (
              <div>
                <div className="fgroup">
                  <label>选择文件（CSV / Excel / ZIP）</label>
                  <input type="file" ref={uploadInputRef} accept=".csv,.xlsx,.xls,.zip" />
                </div>
                <button className="btn-primary btn-sm" onClick={doUpload} disabled={uploadBusy}>上传</button>
                {uploadStatus && (
                  <div className={`status-bar status-${uploadStatus.type}`}>
                    {uploadStatus.spinner && <span className="spinner" />}
                    {uploadStatus.msg}
                  </div>
                )}
              </div>
            )}
          </div>
        </div>

        {/* Step 1: Screening */}
        <div className="card">
          <div className="card-header step">
            第一步：相关性筛选 <span className="step-label">必须步骤，LLM 自动评分</span>
          </div>
          <div className="card-body">
            <div className="row" style={{ marginBottom: 12 }}>
              <div className="fgroup">
                <label>批次大小</label>
                <input type="number" value={batchSize} min={1} max={20} onChange={(e) => setBatchSize(+e.target.value)} />
              </div>
              <div className="fgroup" style={{ paddingTop: 18 }}>
                {sessionLabel && <span className="badge badge-ok">✓ {sessionLabel}</span>}
                <div className="hint">数据就绪后按钮自动启用</div>
              </div>
            </div>
            <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
              <button className="btn-success" onClick={runScreening} disabled={!currentSessionId || screeningBusy}>
                运行相关性筛选
              </button>
              <button className="btn-secondary btn-sm" onClick={skipScreening} disabled={!currentSessionId}>
                跳过筛选，直接获取全文
              </button>
              <span className="hint">已完成筛选的文件可跳过此步骤</span>
            </div>
            {step1Status && (
              <div className={`status-bar status-${step1Status.type}`}>
                {step1Status.spinner && <span className="spinner" />}
                {step1Status.msg}
              </div>
            )}
            {log1.length > 0 && (
              <div className="log">
                {log1.map((line, i) => {
                  const cls = line.startsWith("[ERROR]") ? "log-line-err" :
                              line.startsWith("[!! 警告]") ? "log-line-warn" : "";
                  return <div key={i} className={cls}>{line}</div>;
                })}
              </div>
            )}
            {screenedLabel && (
              <div style={{ marginTop: 10 }}>
                <span className="badge badge-blue">{screenedLabel}</span>
              </div>
            )}
          </div>
        </div>

        {/* Step 2: Fulltext */}
        {screenedSessionId && (
          <div className="card">
            <div className="card-header step2">
              第二步：获取全文 <span className="step-label">筛选完成后可执行</span>
            </div>
            <div className="card-body">
              <div className="warning-banner">
                <strong>⚠ 抓取前必看：</strong>批量直接 HTTP 抓取出版商网站有明确风险。已有用户因抓取速度过快被 Elsevier、Wiley 等查到，<strong>导致整个学校的机构 IP 被封禁</strong>，影响所有师生正常访问。
                <ul>
                  <li>经验值：<strong>单次 &lt; 200 篇基本安全</strong>；超过 200 请分批跑</li>
                  <li>并发数保持 <strong>1–2</strong>，不要图快调到 5+</li>
                  <li>同一批次篇数较多时，建议勾选「浏览器 fallback」走已登录会话</li>
                </ul>
              </div>
              <div className="row" style={{ marginBottom: 12 }}>
                <div className="fgroup">
                  <label>并发数（同时获取文章数）</label>
                  <input type="number" value={fulltextWorkers} min={1} max={10} onChange={(e) => setFulltextWorkers(+e.target.value)} />
                </div>
                <div className="fgroup">
                  <label>浏览器等待时间（秒，启用 fallback 时）</label>
                  <input type="number" value={browserWait} min={5} max={120} onChange={(e) => setBrowserWait(+e.target.value)} />
                </div>
              </div>
              <div className="fgroup">
                <label style={{ display: "inline-flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
                  <input type="checkbox" checked={useBrowser} onChange={(e) => setUseBrowser(e.target.checked)} />
                  使用浏览器 fallback 抓取
                </label>
                <div className="hint">勾选后 HTTP 失败时自动用浏览器重试；浏览器模式建议并发数设为 1-2</div>
              </div>
              <button className="btn-success" onClick={runFulltext} disabled={fulltextBusy}>
                开始获取全文
              </button>
              {step2Status && (
                <div className={`status-bar status-${step2Status.type}`}>
                  {step2Status.spinner && <span className="spinner" />}
                  {step2Status.msg}
                </div>
              )}
              {log2.length > 0 && (
                <div className="log">
                  {log2.map((line, i) => {
                    const cls = line.startsWith("[ERROR]") ? "log-line-err" :
                                line.startsWith("[!! 警告]") ? "log-line-warn" : "";
                    return <div key={i} className={cls}>{line}</div>;
                  })}
                </div>
              )}
              {downloadable && (
                <div style={{ marginTop: 12 }}>
                  <button className="btn-primary" onClick={downloadResult}>下载结果</button>
                </div>
              )}
            </div>
          </div>
        )}

      </div>

      {/* Login modal */}
      <div className={`modal-overlay ${modalOpen ? "active" : ""}`}>
        <div className="modal-box">
          <div className="modal-title">请先登录出版商网站</div>
          <p className="modal-desc">
            检测到以下出版商的文章，请在浏览器中打开并登录（机构网络或账号登录均可），完成后点击「确认已登录，开始获取」。
          </p>
          {modalHasHighRisk && (
            <div className="warning-banner">
              <strong>⚠ 检测到「高风险」出版商：</strong>这类站点对高频抓取识别极敏感（已有真实案例：学校 IP 因批量抓取 Elsevier 被全段封禁）。请务必：
              <ul>
                <li>勾选「使用浏览器 fallback」走已登录会话</li>
                <li>并发数压到 1，不要并行</li>
                <li>每批次 <strong>&lt; 200 篇</strong>，超量请分批</li>
              </ul>
            </div>
          )}
          <div style={{ overflowY: "auto", flex: 1, marginBottom: 4 }}>
            {modalPublishers.map((p, i) => (
              <div className="publisher-item" key={i}>
                <div>
                  <span className="publisher-name">{p.name}</span>
                  <span className="publisher-count">（{p.count} 篇）</span>
                  {publisherRiskClass(p.name) && (
                    <span className={publisherRiskClass(p.name)} style={{ marginLeft: 6 }}>
                      {publisherRiskLabel(p.name)}
                    </span>
                  )}
                </div>
                <button className="btn-primary btn-sm" style={{ whiteSpace: "nowrap", marginLeft: 12 }}
                  onClick={() => openInBrowser(p.url)}>
                  在调试浏览器中打开 ↗
                </button>
              </div>
            ))}
          </div>
          <div className="modal-actions">
            <button className="btn-secondary" onClick={closeModal}>取消</button>
            <button className="btn-success" onClick={proceedFetch}>确认已登录，开始获取</button>
          </div>
        </div>
      </div>
    </div>
  );
}
