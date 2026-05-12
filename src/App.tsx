import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import { open as shellOpen } from "@tauri-apps/plugin-shell";
import {
  Settings2,
  Sparkles,
  Database,
  Workflow,
  Moon,
  Sun,
  Github,
  Upload,
  Search,
  Globe,
  ExternalLink,
  Loader2,
  Download,
  ShieldAlert,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { StatusBar } from "@/components/ui/status-bar";
import { Separator } from "@/components/ui/separator";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ThemeProvider, useTheme } from "@/components/theme-provider";
import { cn } from "@/lib/utils";

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
// Publisher risk classification (drives badge color in login modal)
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

function publisherRiskBadge(name: string) {
  if (PUBLISHER_RISK_HIGH.has(name)) return { variant: "destructive" as const, label: "高风险" };
  if (PUBLISHER_RISK_MEDIUM.has(name)) return { variant: "warning" as const, label: "中风险" };
  return null;
}

// Wait for a task to complete by listening for the "task-done" event.
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
// Sidebar configuration
// ----------------------------------------------------------------------------

type SectionId = "llm" | "query" | "input" | "pipeline";

const SECTIONS: { id: SectionId; title: string; subtitle: string; icon: React.ComponentType<{ className?: string }> }[] = [
  { id: "llm", title: "LLM 配置", subtitle: "API Key / 模型 / 超时", icon: Settings2 },
  { id: "query", title: "AI 检索式", subtitle: "主题 → WoS 表达式 / 规划", icon: Sparkles },
  { id: "input", title: "数据输入", subtitle: "WoS 抓取 或 上传文件", icon: Database },
  { id: "pipeline", title: "处理流程", subtitle: "相关性筛选 + 全文获取", icon: Workflow },
];

// ----------------------------------------------------------------------------
// App root
// ----------------------------------------------------------------------------

export default function AppRoot() {
  return (
    <ThemeProvider>
      <App />
    </ThemeProvider>
  );
}

function App() {
  // LLM config
  const [model, setModel] = useState("gpt-4o-mini");
  const [baseUrl, setBaseUrl] = useState("https://api.openai.com/v1");
  const [apiKey, setApiKey] = useState("");
  const [timeoutSec, setTimeoutSec] = useState(120);

  // Section nav
  const [section, setSection] = useState<SectionId>("llm");

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

  // Data input
  const [inputMode, setInputMode] = useState<"wos" | "upload">("wos");
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

  const llmConfigured = !!(llmConfigArg.model && llmConfigArg.api_key);

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
    setInputMode("wos");
    setSection("input");
    setWosStatus({ msg: "检索式已填入，可直接点击「搜索并抓取」", type: "ok" });
  };

  // -- WoS search --
  const startWosSearchTask = async (query: string, appendSessionId: string): Promise<string> =>
    invoke<string>("wos_search", {
      args: {
        query,
        debug_port: debugPort,
        max_pages: maxPages,
        page_wait_seconds: pageWait,
        append_session_id: appendSessionId,
      },
    });

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
    setSection("input");
    setInputMode("wos");
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

  const openPublisherInDebugBrowser = async (url: string) => {
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

  // --------------------------------------------------------------------------
  // Render
  // --------------------------------------------------------------------------

  const modalHasHighRisk = modalPublishers.some((p) => PUBLISHER_RISK_HIGH.has(p.name));

  return (
    <div className="flex h-screen flex-col overflow-hidden">
      <TopBar repoUrl="https://github.com/zuoliangyu/wos-fetch" />

      <div className="flex flex-1 min-h-0 gap-4 p-4">
        <Sidebar
          current={section}
          onSelect={setSection}
          llmReady={llmConfigured}
          sessionLabel={sessionLabel}
          screenedLabel={screenedLabel}
        />

        <main className="flex-1 min-w-0 overflow-y-auto pr-1">
          <div className="mx-auto max-w-4xl space-y-4 pb-12">
            {section === "llm" && (
              <LlmSection
                model={model} setModel={setModel}
                baseUrl={baseUrl} setBaseUrl={setBaseUrl}
                apiKey={apiKey} setApiKey={setApiKey}
                timeoutSec={timeoutSec} setTimeoutSec={setTimeoutSec}
                llmConfigured={llmConfigured}
              />
            )}

            {section === "query" && (
              <QuerySection
                topic={topic} setTopic={setTopic}
                directionCount={directionCount} setDirectionCount={setDirectionCount}
                oaOnly={oaOnly} setOaOnly={setOaOnly}
                generatedQuery={generatedQuery} setGeneratedQuery={setGeneratedQuery}
                singleQueryVisible={singleQueryVisible}
                plan={plan}
                queryGenStatus={queryGenStatus}
                genQueryBusy={genQueryBusy}
                genPlanBusy={genPlanBusy}
                planSearchRunning={planSearchRunning}
                onGenerateQuery={generateQuery}
                onGeneratePlan={generatePlan}
                onFillQuery={fillQuery}
                onRunPlanAll={runPlanSearchAll}
              />
            )}

            {section === "input" && (
              <InputSection
                mode={inputMode} setMode={setInputMode}
                wosQuery={wosQuery} setWosQuery={setWosQuery}
                debugPort={debugPort} setDebugPort={setDebugPort}
                maxPages={maxPages} setMaxPages={setMaxPages}
                pageWait={pageWait} setPageWait={setPageWait}
                wosStatus={wosStatus}
                wosBusy={wosBusy}
                uploadStatus={uploadStatus}
                uploadBusy={uploadBusy}
                uploadInputRef={uploadInputRef}
                sessionLabel={sessionLabel}
                onSearch={doWosSearch}
                onCheckTargets={checkTargets}
                onLaunchBrowser={launchBrowser}
                onUpload={doUpload}
              />
            )}

            {section === "pipeline" && (
              <PipelineSection
                hasSession={!!currentSessionId}
                sessionLabel={sessionLabel}
                screenedLabel={screenedLabel}
                screenedReady={!!screenedSessionId}
                topic={topic}
                batchSize={batchSize} setBatchSize={setBatchSize}
                step1Status={step1Status}
                log1={log1}
                screeningBusy={screeningBusy}
                fulltextWorkers={fulltextWorkers} setFulltextWorkers={setFulltextWorkers}
                browserWait={browserWait} setBrowserWait={setBrowserWait}
                useBrowser={useBrowser} setUseBrowser={setUseBrowser}
                step2Status={step2Status}
                log2={log2}
                fulltextBusy={fulltextBusy}
                downloadable={downloadable}
                onRunScreening={runScreening}
                onSkipScreening={skipScreening}
                onRunFulltext={runFulltext}
                onDownload={downloadResult}
              />
            )}
          </div>
        </main>
      </div>

      <Footer />

      <Dialog open={modalOpen} onOpenChange={(o) => { if (!o) closeModal(); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>请先登录出版商网站</DialogTitle>
            <DialogDescription>
              检测到以下出版商的文章，请在浏览器中打开并登录（机构网络或账号登录均可），完成后点击「确认已登录，开始获取」。
            </DialogDescription>
          </DialogHeader>

          {modalHasHighRisk && (
            <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 p-3 text-xs text-destructive">
              <ShieldAlert className="h-4 w-4 shrink-0 mt-0.5" />
              <span>
                包含高风险出版商，频繁抓取可能触发反爬。建议改用「仅 OA 期刊」模式或仅获取免费下载的文献。
              </span>
            </div>
          )}

          <div className="space-y-2 max-h-72 overflow-y-auto">
            {modalPublishers.map((p, idx) => {
              const risk = publisherRiskBadge(p.name);
              return (
                <div key={idx} className="flex items-center justify-between rounded-md border border-border bg-card/60 p-3">
                  <div className="flex flex-wrap items-center gap-2 min-w-0">
                    <span className="text-sm font-medium truncate">{p.name}</span>
                    <span className="text-xs text-muted-foreground">× {p.count}</span>
                    {risk && <Badge variant={risk.variant}>{risk.label}</Badge>}
                  </div>
                  <Button size="sm" variant="outline" onClick={() => openPublisherInDebugBrowser(p.url)}>
                    打开 <ExternalLink className="h-3 w-3" />
                  </Button>
                </div>
              );
            })}
          </div>

          <DialogFooter>
            <Button variant="ghost" onClick={closeModal}>取消</Button>
            <Button variant="success" onClick={proceedFetch}>确认已登录，开始获取</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

// ----------------------------------------------------------------------------
// TopBar / Sidebar / Footer
// ----------------------------------------------------------------------------

function TopBar({ repoUrl }: { repoUrl: string }) {
  const { theme, toggle } = useTheme();
  return (
    <header className="surface-toolbar sticky top-0 z-30 flex items-center justify-between gap-4 border-b px-4 py-2.5">
      <div className="flex items-center gap-2">
        <div className="grid h-8 w-8 place-items-center rounded-md bg-primary text-primary-foreground">
          <Search className="h-4 w-4" />
        </div>
        <div className="flex flex-col leading-tight">
          <span className="text-sm font-semibold">wos-fetch</span>
          <span className="text-[10px] text-muted-foreground">Web of Science 文献获取工具</span>
        </div>
      </div>
      <div className="flex items-center gap-1">
        <Button
          variant="ghost"
          size="icon"
          aria-label="GitHub 仓库"
          onClick={() => void shellOpen(repoUrl)}
        >
          <Github className="h-4 w-4" />
        </Button>
        <Button variant="ghost" size="icon" aria-label="切换主题" onClick={toggle}>
          {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
        </Button>
      </div>
    </header>
  );
}

function Sidebar({
  current,
  onSelect,
  llmReady,
  sessionLabel,
  screenedLabel,
}: {
  current: SectionId;
  onSelect: (id: SectionId) => void;
  llmReady: boolean;
  sessionLabel: string;
  screenedLabel: string;
}) {
  return (
    <nav className="surface-sidebar hidden w-60 shrink-0 flex-col rounded-lg p-3 md:flex">
      <ul className="space-y-1">
        {SECTIONS.map((s) => {
          const Icon = s.icon;
          const active = current === s.id;
          return (
            <li key={s.id}>
              <button
                onClick={() => onSelect(s.id)}
                className={cn(
                  "group flex w-full items-start gap-3 rounded-md border border-transparent px-3 py-2 text-left transition-colors",
                  active
                    ? "border-primary/30 bg-primary/10 text-primary shadow-sm"
                    : "hover:bg-accent/60 hover:text-accent-foreground"
                )}
              >
                <Icon className={cn("h-4 w-4 mt-0.5 shrink-0", active ? "text-primary" : "text-muted-foreground")} />
                <div className="flex flex-col min-w-0">
                  <span className="text-sm font-medium leading-tight">{s.title}</span>
                  <span className={cn("text-[11px] leading-tight", active ? "text-primary/70" : "text-muted-foreground")}>{s.subtitle}</span>
                </div>
              </button>
            </li>
          );
        })}
      </ul>

      <Separator className="my-3" />

      <div className="space-y-2 px-1 text-[11px] text-muted-foreground">
        <div className="flex items-center justify-between gap-2">
          <span>LLM</span>
          <Badge variant={llmReady ? "success" : "outline"}>{llmReady ? "就绪" : "未配置"}</Badge>
        </div>
        <div className="flex items-center justify-between gap-2">
          <span>数据</span>
          <span className="truncate text-foreground/80">{sessionLabel || "—"}</span>
        </div>
        <div className="flex items-center justify-between gap-2">
          <span>筛选</span>
          <span className="truncate text-foreground/80">{screenedLabel || "—"}</span>
        </div>
      </div>
    </nav>
  );
}

function Footer() {
  return (
    <footer className="surface-toolbar flex flex-wrap items-center justify-between gap-2 border-t px-4 py-2 text-[11px] text-muted-foreground">
      <span>
        作者：<span className="font-medium text-foreground">左岚</span>
        {" · "}
        <button className="hover:underline" onClick={() => void shellOpen("https://github.com/zuoliangyu")}>
          @zuoliangyu
        </button>
      </span>
      <span>
        <button className="hover:underline" onClick={() => void shellOpen("https://github.com/zuoliangyu/wos-fetch")}>
          github.com/zuoliangyu/wos-fetch
        </button>
      </span>
    </footer>
  );
}

// ----------------------------------------------------------------------------
// Sections
// ----------------------------------------------------------------------------

function LlmSection(props: {
  model: string; setModel: (v: string) => void;
  baseUrl: string; setBaseUrl: (v: string) => void;
  apiKey: string; setApiKey: (v: string) => void;
  timeoutSec: number; setTimeoutSec: (v: number) => void;
  llmConfigured: boolean;
}) {
  const { model, setModel, baseUrl, setBaseUrl, apiKey, setApiKey, timeoutSec, setTimeoutSec, llmConfigured } = props;
  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between gap-2">
          <CardTitle>LLM 配置</CardTitle>
          <Badge variant={llmConfigured ? "success" : "outline"}>
            {llmConfigured ? "就绪" : "未配置"}
          </Badge>
        </div>
        <CardDescription>
          AI 检索式生成 / 相关性筛选会使用此配置。支持 OpenAI 兼容协议（含本地中转 / 各家代理）。
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4 sm:grid-cols-2">
        <Field label="Model" hint="例如 gpt-4o-mini / deepseek-chat / glm-4-air">
          <Input value={model} onChange={(e) => setModel(e.target.value)} placeholder="gpt-4o-mini" />
        </Field>
        <Field label="Base URL" hint="OpenAI 兼容 endpoint，含 /v1">
          <Input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="https://api.openai.com/v1" />
        </Field>
        <Field label="API Key" hint="只保留在本地内存，应用退出即清空" className="sm:col-span-2">
          <Input type="password" value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="sk-..." />
        </Field>
        <Field label="超时（秒）" hint="LLM 调用单次超时；生成完整规划时自动放宽到至少 180 秒">
          <Input
            type="number"
            min={30}
            value={timeoutSec}
            onChange={(e) => setTimeoutSec(Math.max(30, Number(e.target.value) || 0))}
          />
        </Field>
      </CardContent>
    </Card>
  );
}

function QuerySection(props: {
  topic: string; setTopic: (v: string) => void;
  directionCount: string; setDirectionCount: (v: string) => void;
  oaOnly: boolean; setOaOnly: (v: boolean) => void;
  generatedQuery: string; setGeneratedQuery: (v: string) => void;
  singleQueryVisible: boolean;
  plan: Plan | null;
  queryGenStatus: Status | null;
  genQueryBusy: boolean;
  genPlanBusy: boolean;
  planSearchRunning: boolean;
  onGenerateQuery: () => void;
  onGeneratePlan: () => void;
  onFillQuery: (q: string) => void;
  onRunPlanAll: () => void;
}) {
  const {
    topic, setTopic, directionCount, setDirectionCount, oaOnly, setOaOnly,
    generatedQuery, setGeneratedQuery, singleQueryVisible, plan,
    queryGenStatus, genQueryBusy, genPlanBusy, planSearchRunning,
    onGenerateQuery, onGeneratePlan, onFillQuery, onRunPlanAll,
  } = props;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>AI 检索式生成（可选）</CardTitle>
          <CardDescription>把中文研究主题转换成 WoS 高级检索表达式；或生成完整的多方向综述检索规划。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Field label="研究主题描述">
            <Textarea
              value={topic}
              onChange={(e) => setTopic(e.target.value)}
              rows={4}
              placeholder="例如：近五年基于深度学习的医学影像分割方法综述..."
            />
          </Field>

          <div className="flex items-start gap-3 rounded-md border border-border bg-accent/30 p-3">
            <Switch checked={oaOnly} onCheckedChange={setOaOnly} id="oa-only" />
            <div className="flex flex-col gap-0.5">
              <label htmlFor="oa-only" className="text-sm font-medium cursor-pointer">
                仅 OA（开放获取）期刊 — 推荐用于课程论文
              </label>
              <p className={cn("text-xs leading-relaxed", oaOnly ? "text-muted-foreground" : "text-warning-foreground")}>
                {oaOnly
                  ? '已限定 OA 期刊：抓取仅命中开放获取文献，可避免触发 Elsevier / Wiley / Springer 等出版商的反爬机制。'
                  : '⚠ 未限定 OA：可能命中付费墙文献，频繁抓取容易导致 WoS 账号或 IP 被出版商封禁。'}
              </p>
            </div>
          </div>

          <div className="flex flex-wrap items-end gap-3">
            <Field label="检索方向数量（生成完整规划时）" className="flex-1 min-w-[200px]">
              <SelectNative value={directionCount} onChange={(v) => setDirectionCount(v)}>
                <option value="auto">自动（根据主题宽窄）</option>
                <option value="3">3 条</option>
                <option value="5">5 条</option>
                <option value="7">7 条</option>
                <option value="10">10 条</option>
              </SelectNative>
            </Field>
            <Button variant="secondary" onClick={onGenerateQuery} disabled={genQueryBusy}>
              {genQueryBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              生成单条检索式
            </Button>
            <Button onClick={onGeneratePlan} disabled={genPlanBusy}>
              {genPlanBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              生成完整检索规划
            </Button>
          </div>

          {queryGenStatus && (
            <StatusBar variant={queryGenStatus.type} message={queryGenStatus.msg} spinner={queryGenStatus.spinner} />
          )}

          {singleQueryVisible && (
            <Field label="生成的检索式（可手工编辑）">
              <Textarea
                value={generatedQuery}
                onChange={(e) => setGeneratedQuery(e.target.value)}
                rows={4}
                className="font-mono text-xs"
              />
              <div className="mt-2 flex justify-end">
                <Button size="sm" variant="success" onClick={() => onFillQuery(generatedQuery)}>
                  填入检索框 ↓
                </Button>
              </div>
            </Field>
          )}
        </CardContent>
      </Card>

      {plan && (
        <Card>
          <CardHeader>
            <div className="flex items-center justify-between gap-2">
              <div>
                <CardTitle>检索规划（{plan.search_directions.length} 个方向）</CardTitle>
                <CardDescription className="mt-1">
                  {plan.normalized_topic || "—"}{plan.inferred_domain ? ` · ${plan.inferred_domain}` : ""}
                </CardDescription>
              </div>
              <Button size="sm" onClick={onRunPlanAll} disabled={planSearchRunning}>
                {planSearchRunning && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                一键运行全部
              </Button>
            </div>
          </CardHeader>
          <CardContent className="space-y-3">
            {plan.search_directions.map((d, idx) => (
              <div key={idx} className="rounded-md border border-border bg-card/60 p-3 space-y-2">
                <div className="flex items-start justify-between gap-2">
                  <div className="flex items-center gap-2 min-w-0">
                    <span className="text-xs text-muted-foreground shrink-0">#{idx + 1}</span>
                    <span className="text-sm font-medium truncate">{d.direction_name || d.purpose || "方向"}</span>
                  </div>
                  <DirectionStatusBadge status={d.run_status} added={d.added_count} />
                </div>
                {d.purpose && <p className="text-xs text-muted-foreground">{d.purpose}</p>}
                <pre className="overflow-x-auto rounded bg-muted/50 p-2 font-mono text-[11px] leading-relaxed">{d.search_query}</pre>
                <div className="flex justify-end">
                  <Button size="sm" variant="outline" onClick={() => onFillQuery(d.search_query)}>
                    填入检索框
                  </Button>
                </div>
              </div>
            ))}
          </CardContent>
        </Card>
      )}
    </div>
  );
}

function DirectionStatusBadge({ status, added }: { status?: string; added?: number }) {
  if (status === "ok") return <Badge variant="success">+{added ?? 0}</Badge>;
  if (status === "running") return <Badge variant="warning">运行中</Badge>;
  if (status === "err") return <Badge variant="destructive">失败</Badge>;
  return <Badge variant="outline">待运行</Badge>;
}

function InputSection(props: {
  mode: "wos" | "upload"; setMode: (m: "wos" | "upload") => void;
  wosQuery: string; setWosQuery: (v: string) => void;
  debugPort: number; setDebugPort: (v: number) => void;
  maxPages: number; setMaxPages: (v: number) => void;
  pageWait: number; setPageWait: (v: number) => void;
  wosStatus: Status | null;
  wosBusy: boolean;
  uploadStatus: Status | null;
  uploadBusy: boolean;
  uploadInputRef: React.RefObject<HTMLInputElement>;
  sessionLabel: string;
  onSearch: () => void;
  onCheckTargets: () => void;
  onLaunchBrowser: () => void;
  onUpload: () => void;
}) {
  const {
    mode, setMode, wosQuery, setWosQuery, debugPort, setDebugPort, maxPages, setMaxPages,
    pageWait, setPageWait, wosStatus, wosBusy, uploadStatus, uploadBusy, uploadInputRef,
    sessionLabel, onSearch, onCheckTargets, onLaunchBrowser, onUpload,
  } = props;

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between gap-2 flex-wrap">
          <div>
            <CardTitle>数据输入</CardTitle>
            <CardDescription>从 WoS 在线抓取，或上传本地导出的 CSV/Excel/ZIP。</CardDescription>
          </div>
          {sessionLabel && <Badge variant="success">已就绪：{sessionLabel}</Badge>}
        </div>

        <div className="mt-3 inline-flex rounded-md border border-border bg-card/60 p-0.5 text-xs">
          <ModeTab active={mode === "wos"} onClick={() => setMode("wos")} icon={Globe}>WoS 检索</ModeTab>
          <ModeTab active={mode === "upload"} onClick={() => setMode("upload")} icon={Upload}>上传文件</ModeTab>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {mode === "wos" ? (
          <>
            <Field label="WoS 高级检索式">
              <Textarea
                value={wosQuery}
                onChange={(e) => setWosQuery(e.target.value)}
                rows={3}
                placeholder='TS=("deep learning" AND "medical image") AND PY=(2020-2024)'
                className="font-mono text-xs"
              />
            </Field>

            <div className="grid gap-3 sm:grid-cols-3">
              <Field label="调试端口">
                <Input type="number" value={debugPort} onChange={(e) => setDebugPort(Number(e.target.value) || 9222)} />
              </Field>
              <Field label="抓取页数">
                <Input type="number" min={1} value={maxPages} onChange={(e) => setMaxPages(Math.max(1, Number(e.target.value) || 1))} />
              </Field>
              <Field label="每页等待（秒）">
                <Input type="number" min={5} value={pageWait} onChange={(e) => setPageWait(Math.max(5, Number(e.target.value) || 5))} />
              </Field>
            </div>

            <div className="flex flex-wrap gap-2">
              <Button onClick={onSearch} disabled={wosBusy}>
                {wosBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                <Search className="h-3.5 w-3.5" />
                搜索并抓取
              </Button>
              <Button variant="outline" onClick={onCheckTargets}>检测浏览器</Button>
              <Button variant="outline" onClick={onLaunchBrowser}>启动 / 复用浏览器</Button>
            </div>

            {wosStatus && (
              <StatusBar variant={wosStatus.type} message={wosStatus.msg} spinner={wosStatus.spinner} />
            )}
          </>
        ) : (
          <>
            <Field label="选择文件（.csv / .xlsx / .xls / .zip）">
              <input
                type="file"
                ref={uploadInputRef}
                accept=".csv,.xlsx,.xls,.zip"
                className="block w-full text-xs file:mr-3 file:rounded-md file:border-0 file:bg-secondary file:px-3 file:py-1.5 file:text-secondary-foreground file:hover:bg-secondary/80"
              />
            </Field>
            <div>
              <Button onClick={onUpload} disabled={uploadBusy}>
                {uploadBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                <Upload className="h-3.5 w-3.5" />
                上传
              </Button>
            </div>
            {uploadStatus && (
              <StatusBar variant={uploadStatus.type} message={uploadStatus.msg} spinner={uploadStatus.spinner} />
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

function ModeTab({
  active, onClick, icon: Icon, children,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ComponentType<{ className?: string }>;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-1.5 rounded px-3 py-1 transition-colors",
        active ? "bg-primary text-primary-foreground shadow-sm" : "text-muted-foreground hover:text-foreground"
      )}
    >
      <Icon className="h-3.5 w-3.5" />
      {children}
    </button>
  );
}

function PipelineSection(props: {
  hasSession: boolean;
  sessionLabel: string;
  screenedLabel: string;
  screenedReady: boolean;
  topic: string;
  batchSize: number; setBatchSize: (v: number) => void;
  step1Status: Status | null;
  log1: string[];
  screeningBusy: boolean;
  fulltextWorkers: number; setFulltextWorkers: (v: number) => void;
  browserWait: number; setBrowserWait: (v: number) => void;
  useBrowser: boolean; setUseBrowser: (v: boolean) => void;
  step2Status: Status | null;
  log2: string[];
  fulltextBusy: boolean;
  downloadable: string | null;
  onRunScreening: () => void;
  onSkipScreening: () => void;
  onRunFulltext: () => void;
  onDownload: () => void;
}) {
  const {
    hasSession, sessionLabel, screenedLabel, screenedReady,
    batchSize, setBatchSize, step1Status, log1, screeningBusy,
    fulltextWorkers, setFulltextWorkers, browserWait, setBrowserWait,
    useBrowser, setUseBrowser, step2Status, log2, fulltextBusy, downloadable,
    onRunScreening, onSkipScreening, onRunFulltext, onDownload,
  } = props;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-2 flex-wrap">
            <div>
              <CardTitle>第一步：相关性筛选</CardTitle>
              <CardDescription>调用 LLM 对每条记录与你的研究主题进行打分，筛除明显不相关的文献。</CardDescription>
            </div>
            {hasSession ? <Badge variant="success">数据：{sessionLabel}</Badge> : <Badge variant="outline">需先完成数据输入</Badge>}
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="批大小（每次提交给 LLM 的记录数）">
              <Input type="number" min={1} max={50} value={batchSize} onChange={(e) => setBatchSize(Math.max(1, Number(e.target.value) || 1))} />
            </Field>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button onClick={onRunScreening} disabled={!hasSession || screeningBusy}>
              {screeningBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              运行相关性筛选
            </Button>
            <Button variant="outline" onClick={onSkipScreening} disabled={!hasSession}>
              跳过筛选
            </Button>
            {screenedLabel && <Badge variant="success">{screenedLabel}</Badge>}
          </div>
          {step1Status && (
            <StatusBar variant={step1Status.type} message={step1Status.msg} spinner={step1Status.spinner} />
          )}
          {log1.length > 0 && <LogView lines={log1} />}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between gap-2 flex-wrap">
            <div>
              <CardTitle>第二步：全文获取</CardTitle>
              <CardDescription>按 DOI 抓取摘要 / 全文，可选启用调试浏览器对付强反爬站点。</CardDescription>
            </div>
            {screenedReady ? <Badge variant="success">筛选已就绪</Badge> : <Badge variant="outline">需先完成或跳过筛选</Badge>}
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="grid gap-3 sm:grid-cols-3">
            <Field label="并发 worker 数">
              <Input type="number" min={1} max={8} value={fulltextWorkers} onChange={(e) => setFulltextWorkers(Math.max(1, Number(e.target.value) || 1))} />
            </Field>
            <Field label="浏览器单页等待（秒）">
              <Input type="number" min={5} value={browserWait} onChange={(e) => setBrowserWait(Math.max(5, Number(e.target.value) || 5))} />
            </Field>
            <Field label="启用浏览器回退">
              <div className="flex h-9 items-center gap-3 rounded-md border border-border bg-card/70 px-3 text-xs">
                <Switch checked={useBrowser} onCheckedChange={setUseBrowser} />
                <span className="text-muted-foreground">遇到反爬时回退到浏览器</span>
              </div>
            </Field>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button onClick={onRunFulltext} disabled={!screenedReady || fulltextBusy}>
              {fulltextBusy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              获取全文
            </Button>
            {downloadable && (
              <Button variant="success" onClick={onDownload}>
                <Download className="h-3.5 w-3.5" />
                下载结果
              </Button>
            )}
          </div>
          {step2Status && (
            <StatusBar variant={step2Status.type} message={step2Status.msg} spinner={step2Status.spinner} />
          )}
          {log2.length > 0 && <LogView lines={log2} />}
        </CardContent>
      </Card>
    </div>
  );
}

// ----------------------------------------------------------------------------
// Small helpers
// ----------------------------------------------------------------------------

function Field({
  label, hint, children, className,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex flex-col gap-1.5", className)}>
      <Label>{label}</Label>
      {children}
      {hint && <span className="text-[11px] text-muted-foreground">{hint}</span>}
    </div>
  );
}

function SelectNative({
  value, onChange, children,
}: {
  value: string;
  onChange: (v: string) => void;
  children: React.ReactNode;
}) {
  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="flex h-9 w-full rounded-md border border-border bg-card/70 px-2 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
    >
      {children}
    </select>
  );
}

function LogView({ lines }: { lines: string[] }) {
  return (
    <pre className="max-h-48 overflow-y-auto rounded-md border border-border bg-muted/40 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
      {lines.map((l, i) => <div key={i}>{l}</div>)}
    </pre>
  );
}
