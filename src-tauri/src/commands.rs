//! Tauri command handlers — the IPC surface exposed to the React frontend.
//!
//! Port target: `main.py` (~512 lines of FastAPI endpoints). Replaces:
//!
//! - Per-request `body: dict` + manual unpacking → strongly-typed serde args
//! - In-process session / task dicts (`_sessions`, `_tasks`) → AppState
//!   wrapped in `Arc<parking_lot::RwLock<_>>` and stored via `.manage()`
//! - SSE progress streams → Tauri events emitted with `task-progress`
//!   (frontend listens with `listen("task-progress", ...)`)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::core::llm_client::{list_models, validate_llm_connection, LlmConfig};
use crate::core::table_io::{read_table, serialize_result_table, Table};
use crate::core::text_normalize::validate_wos_search_query;
use crate::skills::fulltext_acquisition::{
    acquire_article_fulltext, acquire_article_fulltext_with_browser_fallback,
    get_required_publishers, PublisherSummary, DEFAULT_TIMEOUT_SECONDS, HTML_MIN_CHARS,
};
use crate::skills::query_builder::{build_review_plan, build_wos_search_query};
use crate::skills::relevance_screening::{score_relevance, ScreeningContext};
use crate::skills::wos_browser::scraper::scrape_wos_pages;
use crate::skills::wos_browser::search::run_wos_search;
use crate::skills::wos_browser::tools::{
    get_debug_targets, launch_wos_browser, open_debug_target, DebugTarget, DEFAULT_DEBUG_PORT,
};

const MAX_SESSIONS: usize = 32;
const MAX_TASKS: usize = 64;

// ---------------------------------------------------------------------------
// Application state (managed by Tauri)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub status: String, // "running" | "done" | "error"
    pub error: String,
    pub result_format: String,
    /// Optional successor session IDs surfaced after a task completes.
    pub wos_session_id: String,
    pub screened_session_id: String,
    pub row_count: usize,
    pub added_count: usize,
    pub source_row_count: usize,
    #[serde(skip)]
    pub result_bytes: Vec<u8>,
}

#[derive(Default)]
pub struct AppStateInner {
    pub sessions: HashMap<String, Table>,
    pub tasks: HashMap<String, TaskRecord>,
    /// FIFO key order so we can evict oldest entries.
    pub session_order: Vec<String>,
    pub task_order: Vec<String>,
}

pub type AppState = Arc<Mutex<AppStateInner>>;

pub fn new_app_state() -> AppState {
    Arc::new(Mutex::new(AppStateInner::default()))
}

fn evict_oldest<T>(store: &mut HashMap<String, T>, order: &mut Vec<String>, capacity: usize) {
    while store.len() > capacity {
        if order.is_empty() {
            break;
        }
        let oldest = order.remove(0);
        store.remove(&oldest);
    }
}

fn short_id() -> String {
    let bytes = Uuid::new_v4().as_bytes().to_vec();
    let mut s = String::with_capacity(8);
    for b in &bytes[..4] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Progress event helpers (emitted to the frontend)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ProgressEvent {
    task_id: String,
    message: String,
    timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
struct TaskDoneEvent {
    task_id: String,
    status: String,
    error: String,
    timestamp: i64,
}

fn push_progress(app: &AppHandle, task_id: &str, message: impl Into<String>) {
    let payload = ProgressEvent {
        task_id: task_id.to_string(),
        message: message.into().replace('\n', " ").trim().to_string(),
        timestamp: unix_now(),
    };
    let _ = app.emit("task-progress", payload);
}

fn finish_task(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    result_bytes: Vec<u8>,
    result_format: &str,
    mutate: impl FnOnce(&mut TaskRecord),
) {
    {
        let mut guard = state.lock();
        if let Some(task) = guard.tasks.get_mut(task_id) {
            task.status = "done".into();
            task.result_format = result_format.into();
            task.result_bytes = result_bytes;
            mutate(task);
        }
    }
    let _ = app.emit(
        "task-done",
        TaskDoneEvent {
            task_id: task_id.to_string(),
            status: "done".into(),
            error: String::new(),
            timestamp: unix_now(),
        },
    );
}

fn fail_task(app: &AppHandle, state: &AppState, task_id: &str, error: impl Into<String>) {
    let truncated: String = error.into().chars().take(600).collect();
    let truncated = truncated.replace('\n', " ").trim().to_string();
    {
        let mut guard = state.lock();
        if let Some(task) = guard.tasks.get_mut(task_id) {
            task.status = "error".into();
            task.error = truncated.clone();
        }
    }
    let _ = app.emit(
        "task-done",
        TaskDoneEvent {
            task_id: task_id.to_string(),
            status: "error".into(),
            error: truncated,
            timestamp: unix_now(),
        },
    );
}

fn new_task(state: &AppState) -> String {
    let id = short_id();
    let record = TaskRecord {
        task_id: id.clone(),
        status: "running".into(),
        ..Default::default()
    };
    let mut guard = state.lock();
    guard.tasks.insert(id.clone(), record);
    guard.task_order.push(id.clone());
    let mut taken_order = std::mem::take(&mut guard.task_order);
    evict_oldest(&mut guard.tasks, &mut taken_order, MAX_TASKS);
    guard.task_order = taken_order;
    id
}

fn store_session(state: &AppState, table: Table) -> String {
    let id = short_id();
    let mut guard = state.lock();
    guard.sessions.insert(id.clone(), table);
    guard.session_order.push(id.clone());
    let mut taken_order = std::mem::take(&mut guard.session_order);
    evict_oldest(&mut guard.sessions, &mut taken_order, MAX_SESSIONS);
    guard.session_order = taken_order;
    id
}

fn get_session(state: &AppState, session_id: &str) -> Option<Table> {
    state.lock().sessions.get(session_id).cloned()
}

fn replace_session(state: &AppState, session_id: &str, table: Table) {
    state.lock().sessions.insert(session_id.to_string(), table);
}

// ---------------------------------------------------------------------------
// Shared payload types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub session_id: String,
    pub row_count: usize,
    pub doi_count: usize,
    pub columns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LlmConfigArg {
    pub model: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}

fn default_timeout_seconds() -> u64 {
    120
}

impl From<LlmConfigArg> for LlmConfig {
    fn from(arg: LlmConfigArg) -> Self {
        LlmConfig {
            base_url: arg.base_url,
            api_key: arg.api_key,
            model: arg.model,
            timeout_seconds: arg.timeout_seconds.max(30),
            temperature: None,
        }
    }
}

fn doi_count(table: &Table) -> usize {
    let doi_col = ["DOI", "doi", "DOI_NORMALIZED", "doi_normalized"]
        .into_iter()
        .find(|c| table.has_column(c));
    let Some(col) = doi_col else { return 0 };
    table
        .rows
        .iter()
        .filter(|r| match r.get(col) {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Null) | None => false,
            Some(_) => true,
        })
        .count()
}

fn rows_as_objects(table: &Table) -> Vec<Map<String, Value>> {
    table
        .rows
        .iter()
        .cloned()
        .map(serde_json::Map::from_iter)
        .collect()
}

fn objects_to_table(records: Vec<Map<String, Value>>) -> Table {
    let mut columns: Vec<String> = Vec::new();
    for rec in &records {
        for k in rec.keys() {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let rows: Vec<HashMap<String, Value>> = records
        .into_iter()
        .map(|m| m.into_iter().collect::<HashMap<_, _>>())
        .collect();
    Table { columns, rows }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Health-check used by the frontend at startup.
#[tauri::command]
pub async fn ping() -> Result<String, String> {
    Ok("pong from rust".to_string())
}

/// Upload a CSV / XLSX / ZIP file (already read into a byte buffer on the JS side).
#[tauri::command]
pub async fn upload_file(
    state: State<'_, AppState>,
    filename: String,
    bytes: Vec<u8>,
) -> Result<UploadResponse, String> {
    let table = read_table(&bytes, &filename).map_err(|e| e.to_string())?;
    let row_count = table.nrows();
    let dois = doi_count(&table);
    let columns: Vec<String> = table.columns.iter().take(30).cloned().collect();
    let session_id = store_session(&state.inner().clone(), table);
    Ok(UploadResponse {
        session_id,
        row_count,
        doi_count: dois,
        columns,
    })
}

/// List the browser's current debug targets (page-type only).
#[tauri::command]
pub async fn wos_targets(body: Option<HashMap<String, Value>>) -> Result<Vec<DebugTarget>, String> {
    let port = body
        .as_ref()
        .and_then(|b| b.get("debug_port"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEBUG_PORT as u64) as u16;
    let targets = get_debug_targets(port).await.map_err(|e| e.to_string())?;
    Ok(targets
        .into_iter()
        .filter(|t| t.kind == "page")
        .take(10)
        .collect())
}

/// Launch (or reuse) a Chromium debug session.
#[tauri::command]
pub async fn wos_launch(
    app: AppHandle,
    state: State<'_, AppState>,
    body: Option<HashMap<String, Value>>,
) -> Result<String, String> {
    let port = body
        .as_ref()
        .and_then(|b| b.get("debug_port"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEBUG_PORT as u64) as u16;
    let state_clone = state.inner().clone();
    let task_id = new_task(&state_clone);
    let task_id_for_spawn = task_id.clone();
    tokio::spawn(async move {
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("尝试连接或启动浏览器（端口 {port}）..."),
        );
        match launch_wos_browser(None, None, None, port, 20.0).await {
            Ok(info) => {
                push_progress(
                    &app,
                    &task_id_for_spawn,
                    format!("浏览器已就绪：{}", info.browser_name),
                );
                finish_task(
                    &app,
                    &state_clone,
                    &task_id_for_spawn,
                    Vec::new(),
                    "json",
                    |_| {},
                );
            }
            Err(err) => fail_task(&app, &state_clone, &task_id_for_spawn, err.to_string()),
        }
    });
    Ok(task_id)
}

#[derive(Debug, Deserialize)]
pub struct WosSearchArgs {
    pub query: String,
    #[serde(default = "default_debug_port_u16")]
    pub debug_port: u16,
    #[serde(default = "default_max_pages")]
    pub max_pages: u32,
    #[serde(default = "default_page_wait")]
    pub page_wait_seconds: f64,
    #[serde(default)]
    pub append_session_id: String,
}

fn default_debug_port_u16() -> u16 {
    DEFAULT_DEBUG_PORT
}

fn default_max_pages() -> u32 {
    5
}

fn default_page_wait() -> f64 {
    20.0
}

/// Submit a WoS search and scrape results.
#[tauri::command]
pub async fn wos_search(
    app: AppHandle,
    state: State<'_, AppState>,
    args: WosSearchArgs,
) -> Result<String, String> {
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return Err("query required".into());
    }
    let query = validate_wos_search_query(&query).map_err(|e| e.to_string())?;
    if !args.append_session_id.is_empty()
        && get_session(&state.inner().clone(), &args.append_session_id).is_none()
    {
        return Err("append_session_id 无效".into());
    }
    let state_clone = state.inner().clone();
    let task_id = new_task(&state_clone);
    let task_id_for_spawn = task_id.clone();
    let port = args.debug_port;
    let max_pages = args.max_pages;
    let page_wait = args.page_wait_seconds;
    let append_session = args.append_session_id;

    tokio::spawn(async move {
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("提交检索式到端口 {port}..."),
        );
        if let Err(err) = run_wos_search(port, &query, 30.0).await {
            fail_task(&app, &state_clone, &task_id_for_spawn, err.to_string());
            return;
        }
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("检索式已提交，开始抓取（最多 {max_pages} 页，每页等待最长 {page_wait}s）..."),
        );
        let scrape = match scrape_wos_pages(port, max_pages, page_wait, 2, 0).await {
            Ok(r) => r,
            Err(err) => {
                fail_task(&app, &state_clone, &task_id_for_spawn, err.to_string());
                return;
            }
        };
        let record_count = scrape.records.len();
        push_progress(
            &app,
            &task_id_for_spawn,
            format!(
                "抓取完成：{} 页，共 {} 条记录",
                scrape.pages_scraped, record_count
            ),
        );
        if scrape.records.is_empty() {
            fail_task(
                &app,
                &state_clone,
                &task_id_for_spawn,
                "未抓取到任何记录，请确认 WoS 结果页面已加载".to_string(),
            );
            return;
        }
        let records: Vec<Map<String, Value>> = scrape
            .records
            .into_iter()
            .filter_map(|v| v.as_object().cloned())
            .collect();
        let source_row_count = records.len();
        let mut new_table = objects_to_table(records);
        let mut session_id = String::new();
        let mut added_count = source_row_count;

        if !append_session.is_empty() {
            if let Some(existing) = get_session(&state_clone, &append_session) {
                let mut combined_rows: Vec<Map<String, Value>> = rows_as_objects(&existing);
                combined_rows.extend(rows_as_objects(&new_table));
                let merged_table = objects_to_table(combined_rows);
                added_count = merged_table.nrows().saturating_sub(existing.nrows());
                replace_session(&state_clone, &append_session, merged_table.clone());
                new_table = merged_table;
                session_id = append_session.clone();
            }
        }
        if session_id.is_empty() {
            session_id = store_session(&state_clone, new_table.clone());
        }

        let final_count = new_table.nrows();
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("[WOS_SESSION:{session_id}] {final_count} 条"),
        );
        finish_task(
            &app,
            &state_clone,
            &task_id_for_spawn,
            Vec::new(),
            "wos",
            |task| {
                task.wos_session_id = session_id;
                task.row_count = final_count;
                task.added_count = added_count;
                task.source_row_count = source_row_count;
            },
        );
    });
    Ok(task_id)
}

#[derive(Debug, Deserialize)]
pub struct ScreeningArgs {
    pub session_id: String,
    pub llm: LlmConfigArg,
    #[serde(default)]
    pub topic: String,
    #[serde(default = "default_screening_batch_size")]
    pub batch_size: usize,
}

fn default_screening_batch_size() -> usize {
    12
}

/// Run relevance screening on an uploaded / scraped session.
#[tauri::command]
pub async fn run_screening(
    app: AppHandle,
    state: State<'_, AppState>,
    args: ScreeningArgs,
) -> Result<String, String> {
    let table = get_session(&state.inner().clone(), &args.session_id)
        .ok_or_else(|| "session_id 无效，请先上传文件或完成 WoS 抓取".to_string())?;
    let state_clone = state.inner().clone();
    let task_id = new_task(&state_clone);
    let task_id_for_spawn = task_id.clone();
    let llm_config: LlmConfig = args.llm.into();
    let context = ScreeningContext {
        topic: args.topic,
        ..Default::default()
    };
    let batch_size = args.batch_size.clamp(1, 20);

    tokio::spawn(async move {
        let total = table.nrows();
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("开始相关性筛选，共 {total} 条记录..."),
        );
        let records = rows_as_objects(&table);
        if llm_config.model.trim().is_empty() || llm_config.api_key.trim().is_empty() {
            push_progress(
                &app,
                &task_id_for_spawn,
                "未配置 LLM，跳过评分，保留全部记录",
            );
            let kept_table = objects_to_table(records);
            let session_id = store_session(&state_clone, kept_table.clone());
            let kept = kept_table.nrows();
            push_progress(
                &app,
                &task_id_for_spawn,
                format!("[SCREENED_SESSION:{session_id}] {kept} 条"),
            );
            finish_task(
                &app,
                &state_clone,
                &task_id_for_spawn,
                Vec::new(),
                "screening",
                |task| {
                    task.screened_session_id = session_id;
                    task.row_count = kept;
                },
            );
            return;
        }
        let scored = match score_relevance(&records, &context, &llm_config, batch_size).await {
            Ok(s) => s,
            Err(err) => {
                fail_task(&app, &state_clone, &task_id_for_spawn, err.to_string());
                return;
            }
        };
        let kept_records: Vec<Map<String, Value>> = scored
            .into_iter()
            .filter(|r| {
                let total_score = r
                    .get("主题相关性总分")
                    .and_then(|v| match v {
                        Value::String(s) => s.parse::<f64>().ok(),
                        Value::Number(n) => n.as_f64(),
                        _ => None,
                    })
                    .unwrap_or(0.0);
                total_score as i64 >= 40
            })
            .collect();
        let kept = kept_records.len();
        let excluded = total - kept;
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("筛选完成：保留 {kept} / {total}（排除 {excluded}）"),
        );
        if kept_records.is_empty() {
            fail_task(
                &app,
                &state_clone,
                &task_id_for_spawn,
                "筛选后无剩余记录".to_string(),
            );
            return;
        }
        let kept_table = objects_to_table(kept_records);
        let session_id = store_session(&state_clone, kept_table.clone());
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("[SCREENED_SESSION:{session_id}] {kept} 条"),
        );
        finish_task(
            &app,
            &state_clone,
            &task_id_for_spawn,
            Vec::new(),
            "screening",
            |task| {
                task.screened_session_id = session_id;
                task.row_count = kept;
            },
        );
    });
    Ok(task_id)
}

#[derive(Debug, Deserialize)]
pub struct BrowserOpenArgs {
    pub url: String,
    #[serde(default = "default_debug_port_u16")]
    pub debug_port: u16,
}

#[derive(Debug, Serialize)]
pub struct BrowserOpenResponse {
    pub ok: bool,
    pub target_id: String,
    pub error: String,
}

#[tauri::command]
pub async fn browser_open(args: BrowserOpenArgs) -> Result<BrowserOpenResponse, String> {
    if args.url.trim().is_empty() {
        return Err("url required".into());
    }
    match open_debug_target(&args.url, args.debug_port).await {
        Ok(target) => Ok(BrowserOpenResponse {
            ok: true,
            target_id: target.id,
            error: String::new(),
        }),
        Err(err) => Ok(BrowserOpenResponse {
            ok: false,
            target_id: String::new(),
            error: err.to_string().chars().take(300).collect(),
        }),
    }
}

#[derive(Debug, Serialize)]
pub struct PublishersResponse {
    pub publishers: Vec<PublisherSummary>,
}

#[tauri::command]
pub async fn run_fulltext_publishers(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<PublishersResponse, String> {
    let table = get_session(&state.inner().clone(), &session_id)
        .ok_or_else(|| "session_id 无效".to_string())?;
    let doi_col = ["DOI", "doi", "DOI_NORMALIZED", "doi_normalized"]
        .into_iter()
        .find(|c| table.has_column(c));
    let Some(col) = doi_col else {
        return Ok(PublishersResponse {
            publishers: Vec::new(),
        });
    };
    let dois: Vec<String> = table
        .rows
        .iter()
        .filter_map(|r| match r.get(col) {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Some(Value::Null) | None => None,
            Some(other) => {
                let text = other.to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
        })
        .collect();
    Ok(PublishersResponse {
        publishers: get_required_publishers(&dois),
    })
}

#[derive(Debug, Deserialize)]
pub struct FulltextArgs {
    pub session_id: String,
    #[serde(default = "default_fulltext_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_debug_port_u16")]
    pub debug_port: u16,
    #[serde(default)]
    pub use_browser: bool,
    #[serde(default = "default_browser_wait")]
    pub browser_wait_seconds: f64,
    #[serde(default = "default_workers")]
    pub workers: usize,
}

fn default_fulltext_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

fn default_browser_wait() -> f64 {
    25.0
}

fn default_workers() -> usize {
    2
}

#[tauri::command]
pub async fn run_fulltext(
    app: AppHandle,
    state: State<'_, AppState>,
    args: FulltextArgs,
) -> Result<String, String> {
    let table = get_session(&state.inner().clone(), &args.session_id)
        .ok_or_else(|| "session_id 无效".to_string())?;
    let state_clone = state.inner().clone();
    let task_id = new_task(&state_clone);
    let task_id_for_spawn = task_id.clone();
    let workers = args.workers.clamp(1, 10);

    tokio::spawn(async move {
        let total = table.nrows();
        let mode = if args.use_browser {
            "HTTP + 浏览器 fallback"
        } else {
            "HTTP"
        };
        push_progress(
            &app,
            &task_id_for_spawn,
            format!("开始全文获取（{total} 条，模式={mode}，并发={workers}）..."),
        );
        if total > 200 {
            push_progress(
                &app,
                &task_id_for_spawn,
                format!("[!! 警告] 本批次 {total} 条，超过经验安全阈值 200 条；建议中断后分批跑，每批 ≤ 200，批与批之间间隔几分钟，否则 Elsevier / Wiley 等可能识别为批量爬取并封禁机构 IP。"),
            );
        }
        if workers > 2 {
            push_progress(
                &app,
                &task_id_for_spawn,
                format!("[!! 警告] 并发数 {workers} 偏高（建议 1-2）；每个域名后端已自动限流到 1 QPS，但跨域名仍可能触发风控。"),
            );
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(workers));
        let mut handles: Vec<tokio::task::JoinHandle<(usize, Map<String, Value>)>> =
            Vec::with_capacity(total);
        for (idx, raw) in table.rows.iter().enumerate() {
            let mut record_map: Map<String, Value> =
                raw.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let doi = record_map
                .get("DOI")
                .or_else(|| record_map.get("doi"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let sem = semaphore.clone();
            let app_clone = app.clone();
            let task_id_for_inner = task_id_for_spawn.clone();
            let use_browser = args.use_browser;
            let timeout_seconds = args.timeout_seconds.max(30);
            let debug_port = args.debug_port;
            let browser_wait = args.browser_wait_seconds;
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.ok();
                if doi.is_empty() {
                    record_map.insert(
                        "acquisition_status".into(),
                        Value::String("skipped_no_doi".into()),
                    );
                    push_progress(
                        &app_clone,
                        &task_id_for_inner,
                        format!("[{}/{total}] 无DOI，跳过", idx + 1),
                    );
                    return (idx, record_map);
                }
                let acq = if use_browser {
                    acquire_article_fulltext_with_browser_fallback(
                        &doi,
                        debug_port,
                        timeout_seconds,
                        HTML_MIN_CHARS,
                    )
                    .await
                } else {
                    acquire_article_fulltext(&doi, timeout_seconds, HTML_MIN_CHARS).await
                };
                if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(&acq) {
                    for (k, v) in map {
                        record_map.insert(k, v);
                    }
                }
                let _ = browser_wait;
                let status = record_map
                    .get("acquisition_status")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let acq_type = record_map
                    .get("acquisition_type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                push_progress(
                    &app_clone,
                    &task_id_for_inner,
                    format!(
                        "[{}/{total}] {} → {status} ({acq_type})",
                        idx + 1,
                        doi.chars().take(50).collect::<String>()
                    ),
                );
                (idx, record_map)
            }));
        }

        let mut result_map: HashMap<usize, Map<String, Value>> = HashMap::new();
        for handle in handles {
            if let Ok((idx, record)) = handle.await {
                result_map.insert(idx, record);
            }
        }
        let mut result_records: Vec<Map<String, Value>> = (0..total)
            .map(|i| result_map.remove(&i).unwrap_or_default())
            .collect();
        let ok_count = result_records
            .iter()
            .filter(|r| {
                r.get("acquisition_status")
                    .and_then(Value::as_str)
                    .map(|s| s == "ok")
                    .unwrap_or(false)
            })
            .count();

        let out_table = objects_to_table(std::mem::take(&mut result_records));
        match serialize_result_table(&out_table) {
            Ok((bytes, _packaged, fmt, count)) => {
                push_progress(
                    &app,
                    &task_id_for_spawn,
                    format!(
                        "全文获取完成：{ok_count}/{total} 成功，{count} 篇已保存全文，格式={fmt}"
                    ),
                );
                finish_task(
                    &app,
                    &state_clone,
                    &task_id_for_spawn,
                    bytes,
                    &fmt,
                    |task| {
                        task.row_count = total;
                    },
                );
            }
            Err(err) => {
                fail_task(&app, &state_clone, &task_id_for_spawn, err.to_string());
            }
        }
    });
    Ok(task_id)
}

#[tauri::command]
pub async fn task_status(
    state: State<'_, AppState>,
    task_id: String,
) -> Result<TaskRecord, String> {
    state
        .lock()
        .tasks
        .get(&task_id)
        .cloned()
        .ok_or_else(|| "task not found".to_string())
}

#[derive(Debug, Serialize)]
pub struct TaskExportMeta {
    pub filename: String,
    pub ext: String,
}

#[tauri::command]
pub fn task_export_meta(
    state: State<'_, AppState>,
    task_id: String,
) -> Result<TaskExportMeta, String> {
    let task = state
        .lock()
        .tasks
        .get(&task_id)
        .cloned()
        .ok_or_else(|| "task not found".to_string())?;
    if task.result_bytes.is_empty() {
        return Err("结果未就绪".into());
    }
    let ext = if task.result_format == "zip" {
        "zip"
    } else {
        "xlsx"
    };
    Ok(TaskExportMeta {
        filename: format!("fetch_result.{ext}"),
        ext: ext.into(),
    })
}

#[tauri::command]
pub fn task_save_to(
    state: State<'_, AppState>,
    task_id: String,
    dest: String,
) -> Result<(), String> {
    let bytes = {
        let guard = state.lock();
        let task = guard
            .tasks
            .get(&task_id)
            .ok_or_else(|| "task not found".to_string())?;
        if task.result_bytes.is_empty() {
            return Err("结果未就绪".into());
        }
        task.result_bytes.clone()
    };
    std::fs::write(&dest, &bytes).map_err(|e| format!("写入失败：{e}"))
}

#[derive(Debug, Deserialize)]
pub struct GenerateQueryArgs {
    pub topic: String,
    pub llm: LlmConfigArg,
    #[serde(default = "default_oa_only")]
    pub oa_only: bool,
}

fn default_oa_only() -> bool {
    true
}

#[tauri::command]
pub async fn generate_query(args: GenerateQueryArgs) -> Result<String, String> {
    let topic = args.topic.trim();
    if topic.is_empty() {
        return Err("请输入研究主题".into());
    }
    let llm_config: LlmConfig = args.llm.into();
    if llm_config.model.trim().is_empty() || llm_config.api_key.trim().is_empty() {
        return Err("请配置 model 和 api_key".into());
    }
    build_wos_search_query(topic, &llm_config, args.oa_only)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct GeneratePlanArgs {
    pub topic: String,
    pub llm: LlmConfigArg,
    #[serde(default = "default_direction_count")]
    pub direction_count: String,
    #[serde(default = "default_oa_only")]
    pub oa_only: bool,
}

fn default_direction_count() -> String {
    "auto".into()
}

#[tauri::command]
pub async fn generate_plan(args: GeneratePlanArgs) -> Result<Value, String> {
    let topic = args.topic.trim();
    if topic.is_empty() {
        return Err("请输入研究主题".into());
    }
    let llm_config: LlmConfig = args.llm.into();
    if llm_config.model.trim().is_empty() || llm_config.api_key.trim().is_empty() {
        return Err("请配置 model 和 api_key".into());
    }
    build_review_plan(topic, &llm_config, &args.direction_count, args.oa_only)
        .await
        .map_err(|e| e.to_string())
}

/// Validate an LLM endpoint by sending a one-shot `ping` chat message.
#[tauri::command]
pub async fn validate_llm(args: LlmConfigArg) -> Result<String, String> {
    let llm_config: LlmConfig = args.into();
    validate_llm_connection(&llm_config)
        .await
        .map_err(|e| e.to_string())
}

/// Probe an OpenAI-compatible endpoint for the list of model ids. The `model`
/// field on the arg is unused — this only needs base_url + api_key.
#[tauri::command]
pub async fn scan_models(args: LlmConfigArg) -> Result<Vec<String>, String> {
    let mut llm_config: LlmConfig = args.into();
    if llm_config.model.trim().is_empty() {
        llm_config.model = "placeholder".into();
    }
    list_models(&llm_config).await.map_err(|e| e.to_string())
}
