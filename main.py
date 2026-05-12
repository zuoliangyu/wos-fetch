from __future__ import annotations

import asyncio
import queue as queue_module
import threading
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

import pandas as pd
from fastapi import FastAPI, File, HTTPException, UploadFile
from fastapi.responses import HTMLResponse, JSONResponse, Response, StreamingResponse

from core.table_io import read_table, serialize_result_table
from core.text_normalize import validate_wos_search_query
from skills.fulltext_acquisition import acquire_article_fulltext_with_browser_fallback, get_required_publishers
from skills.query_builder import build_review_plan, build_wos_search_query
from skills.relevance_screening import score_relevance
from skills.wos_browser import (
    DEFAULT_DEBUG_PORT,
    inspect_browser_targets,
    launch_wos_browser,
    submit_wos_search,
)
from skills.wos_browser.tools import open_debug_target
from skills.wos_browser.scraper import scrape_wos_pages

STATIC_DIR = Path(__file__).parent / "static"
app = FastAPI(title="wos-fetch")

_sessions: dict[str, pd.DataFrame] = {}
_tasks: dict[str, dict[str, Any]] = {}
_lock = threading.Lock()

_MAX_TASKS = 64
_MAX_SESSIONS = 32


def _evict_oldest(store: dict[str, Any], capacity: int) -> None:
    while len(store) > capacity:
        oldest_key = next(iter(store))
        store.pop(oldest_key, None)


def _new_task() -> tuple[str, dict[str, Any]]:
    task_id = uuid.uuid4().hex[:8]
    task: dict[str, Any] = {"status": "running", "queue": queue_module.Queue(),
                             "result": None, "result_format": "", "error": ""}
    with _lock:
        _tasks[task_id] = task
        _evict_oldest(_tasks, _MAX_TASKS)
    return task_id, task


def _push(task: dict, msg: str) -> None:
    task["queue"].put(msg.replace("\n", " ").strip())


def _finish(task: dict, result: bytes, result_format: str) -> None:
    task["result"] = result
    task["result_format"] = result_format
    task["status"] = "done"
    task["queue"].put(None)


def _fail(task: dict, error: str) -> None:
    task["error"] = error.replace("\n", " ").strip()
    task["status"] = "error"
    task["queue"].put(None)


def _find_column(df: pd.DataFrame, aliases: list[str]) -> str:
    columns = {str(col).strip().lower(): str(col) for col in df.columns}
    for alias in aliases:
        matched = columns.get(alias.strip().lower())
        if matched:
            return matched
    return ""


def _normalize_key(value: Any) -> str:
    text = str(value or "").strip().lower()
    return " ".join(text.split())


def _deduplicate_session_rows(df: pd.DataFrame) -> pd.DataFrame:
    if df.empty:
        return df

    work = df.copy()
    dedup_key = pd.Series("", index=work.index, dtype="object")

    doi_col = _find_column(work, ["DOI", "DOI_NORMALIZED"])
    ut_col = _find_column(work, ["UT", "Accession Number", "accession_number"])
    title_col = _find_column(work, ["Article Title", "Title", "标题"])

    if doi_col:
        doi_key = work[doi_col].map(_normalize_key)
        dedup_key = dedup_key.mask(doi_key.ne(""), "doi:" + doi_key)
    if ut_col:
        ut_key = work[ut_col].map(_normalize_key)
        dedup_key = dedup_key.mask(dedup_key.eq("") & ut_key.ne(""), "ut:" + ut_key)
    if title_col:
        title_key = work[title_col].map(_normalize_key)
        dedup_key = dedup_key.mask(dedup_key.eq("") & title_key.ne(""), "title:" + title_key)

    fallback_key = pd.Series([f"row:{i}" for i in range(len(work))], index=work.index, dtype="object")
    work["__dedup_key"] = dedup_key.mask(dedup_key.eq(""), fallback_key)
    work = work.drop_duplicates(subset="__dedup_key", keep="first").drop(columns="__dedup_key")
    return work.reset_index(drop=True)


async def _sse_generator(task: dict):
    q: queue_module.Queue = task["queue"]
    while True:
        try:
            msg = q.get_nowait()
        except queue_module.Empty:
            await asyncio.sleep(0.25)
            continue
        if msg is None:
            if task["status"] == "error":
                yield f"data: [ERROR] {task['error']}\n\n"
            yield "data: [DONE]\n\n"
            break
        yield f"data: {msg}\n\n"


# ── 静态页面 ──────────────────────────────────────────────────────────────

@app.get("/", response_class=HTMLResponse)
async def index():
    return (STATIC_DIR / "index.html").read_text(encoding="utf-8")


# ── 文件上传 ──────────────────────────────────────────────────────────────

@app.post("/api/upload")
async def upload(file: UploadFile = File(...)):
    data = await file.read()
    try:
        df = read_table(data, file.filename or "upload.csv")
    except Exception as exc:
        raise HTTPException(400, str(exc))
    session_id = uuid.uuid4().hex[:8]
    with _lock:
        _sessions[session_id] = df
        _evict_oldest(_sessions, _MAX_SESSIONS)
    doi_col = next((c for c in df.columns if str(c).upper() in {"DOI", "DOI_NORMALIZED"}), "")
    doi_count = int(df[doi_col].notna().sum()) if doi_col else 0
    return JSONResponse({"session_id": session_id, "row_count": len(df),
                         "doi_count": doi_count, "columns": list(df.columns)[:30]})


# ── 浏览器控制 ────────────────────────────────────────────────────────────

@app.post("/api/wos/targets")
async def wos_targets(body: dict = {}):
    port = int(body.get("debug_port", DEFAULT_DEBUG_PORT))
    try:
        targets = inspect_browser_targets(port=port)
    except Exception as exc:
        return JSONResponse({"ok": False, "error": str(exc), "targets": []})
    page_targets = [t for t in targets if t.get("type") == "page"]
    return JSONResponse({"ok": True, "targets": page_targets[:10]})


@app.post("/api/wos/launch")
async def wos_launch(body: dict = {}):
    port = int(body.get("debug_port", DEFAULT_DEBUG_PORT))
    task_id, task = _new_task()

    def _run():
        try:
            _push(task, f"尝试连接或启动浏览器（端口 {port}）...")
            info = launch_wos_browser(port=port)
            _push(task, f"浏览器已就绪：{info.get('browser_name', 'unknown')}")
            task["launch_info"] = info
            _finish(task, b"", "json")
        except Exception as exc:
            _fail(task, str(exc)[:600])

    threading.Thread(target=_run, daemon=True).start()
    return JSONResponse({"task_id": task_id})


# ── WoS 搜索 + 抓取 ───────────────────────────────────────────────────────

@app.post("/api/wos/search")
async def wos_search(body: dict):
    query = str(body.get("query", "")).strip()
    port = int(body.get("debug_port", DEFAULT_DEBUG_PORT))
    max_pages = int(body.get("max_pages", 5))
    page_wait_seconds = float(body.get("page_wait_seconds", 20.0))
    append_session_id = str(body.get("append_session_id", "")).strip()
    if not query:
        raise HTTPException(400, "query required")
    try:
        query = validate_wos_search_query(query)
    except ValueError as exc:
        raise HTTPException(400, str(exc))
    if append_session_id:
        with _lock:
            if append_session_id not in _sessions:
                raise HTTPException(400, "append_session_id 无效")

    task_id, task = _new_task()

    def _run():
        try:
            _push(task, f"提交检索式到端口 {port}...")
            submit_wos_search(query=query, port=port)
            _push(task, f"检索式已提交，开始抓取（最多 {max_pages} 页，每页等待最长 {page_wait_seconds}s）...")
            result = scrape_wos_pages(port=port, max_pages=max_pages, wait_seconds=page_wait_seconds)
            records = result.get("records") or []
            pages_scraped = result.get("pages_scraped", 0)
            _push(task, f"抓取完成：{pages_scraped} 页，共 {len(records)} 条记录")
            if not records:
                _fail(task, "未抓取到任何记录，请确认 WoS 结果页面已加载")
                return
            df = pd.DataFrame(records)
            source_row_count = len(df)
            added_count = source_row_count
            with _lock:
                if append_session_id:
                    existing_df = _sessions.get(append_session_id)
                    if existing_df is None:
                        _fail(task, "append_session_id 无效，请重新开始批量检索")
                        return
                    df = _deduplicate_session_rows(pd.concat([existing_df, df], ignore_index=True, sort=False))
                    session_id = append_session_id
                    added_count = max(0, len(df) - len(existing_df))
                else:
                    session_id = uuid.uuid4().hex[:8]
                _sessions[session_id] = df
                _evict_oldest(_sessions, _MAX_SESSIONS)
            task["wos_session_id"] = session_id
            task["row_count"] = len(df)
            task["added_count"] = added_count
            task["source_row_count"] = source_row_count
            if append_session_id:
                _push(task, f"已合并到既有结果：本次新增 {added_count} 条，合并后共 {len(df)} 条")
            _push(task, f"[WOS_SESSION:{session_id}] {len(df)} 条")
            _finish(task, b"", "wos")
        except Exception as exc:
            _fail(task, str(exc)[:600])

    threading.Thread(target=_run, daemon=True).start()
    return JSONResponse({"task_id": task_id})


# ── Step 1：相关性筛选（必须流程，无开关）────────────────────────────────

@app.post("/api/run-screening")
async def run_screening(body: dict):
    session_id = str(body.get("session_id", "")).strip()
    with _lock:
        df = _sessions.get(session_id)
    if df is None:
        raise HTTPException(400, "session_id 无效，请先上传文件或完成 WoS 抓取")

    model = str(body.get("model", "")).strip()
    base_url = str(body.get("base_url", "https://api.openai.com/v1")).strip() or "https://api.openai.com/v1"
    api_key = str(body.get("api_key", "")).strip()
    timeout = max(30, int(body.get("timeout", 120)))
    batch_size = max(1, min(int(body.get("batch_size", 12)), 20))
    topic = str(body.get("topic", "")).strip()

    task_id, task = _new_task()

    def _run():
        try:
            records = df.to_dict("records")
            total = len(records)
            _push(task, f"开始相关性筛选，共 {total} 条记录...")

            if model and api_key:
                context = {"topic_text": topic}
                records = score_relevance(
                    records, context,
                    model=model, base_url=base_url, api_key=api_key,
                    timeout=timeout, batch_size=batch_size,
                )
                kept = [r for r in records
                        if int(float(str(r.get("主题相关性总分") or 0) or 0)) >= 40]
                excluded = total - len(kept)
                _push(task, f"筛选完成：保留 {len(kept)} / {total}（排除 {excluded}）")
                records = kept
            else:
                _push(task, "未配置 LLM，跳过评分，保留全部记录")

            if not records:
                _fail(task, "筛选后无剩余记录")
                return

            screened_df = pd.DataFrame(records)
            screened_session_id = uuid.uuid4().hex[:8]
            with _lock:
                _sessions[screened_session_id] = screened_df
                _evict_oldest(_sessions, _MAX_SESSIONS)
            task["screened_session_id"] = screened_session_id
            task["row_count"] = len(screened_df)
            _push(task, f"[SCREENED_SESSION:{screened_session_id}] {len(screened_df)} 条")
            _finish(task, b"", "screening")
        except Exception as exc:
            import traceback
            _fail(task, f"{exc} | {traceback.format_exc()[-500:]}")

    threading.Thread(target=_run, daemon=True).start()
    return JSONResponse({"task_id": task_id})


# ── Step 2 预检：返回所需出版商列表 ──────────────────────────────────────

@app.post("/api/browser/open")
async def browser_open(body: dict):
    url = str(body.get("url", "")).strip()
    port = int(body.get("debug_port", DEFAULT_DEBUG_PORT))
    if not url:
        raise HTTPException(400, "url required")
    try:
        target = open_debug_target(url, port=port)
        return JSONResponse({"ok": True, "target_id": target.get("id", "")})
    except Exception as exc:
        return JSONResponse({"ok": False, "error": str(exc)[:300]})


@app.post("/api/run-fulltext/publishers")
async def run_fulltext_publishers(body: dict):
    session_id = str(body.get("session_id", "")).strip()
    with _lock:
        df = _sessions.get(session_id)
    if df is None:
        raise HTTPException(400, "session_id 无效")
    doi_col = next((c for c in df.columns if str(c).upper() in {"DOI", "DOI_NORMALIZED"}), "")
    if not doi_col:
        return JSONResponse({"publishers": []})
    dois = [str(v).strip() for v in df[doi_col].dropna() if str(v).strip()]
    publishers = get_required_publishers(dois)
    return JSONResponse({"publishers": publishers})


# ── Step 2：全文获取（HTTP + 可选浏览器 fallback）────────────────────────

@app.post("/api/run-fulltext")
async def run_fulltext(body: dict):
    session_id = str(body.get("session_id", "")).strip()
    with _lock:
        df = _sessions.get(session_id)
    if df is None:
        raise HTTPException(400, "session_id 无效")

    timeout = max(30, int(body.get("timeout", 60)))
    debug_port = int(body.get("debug_port", DEFAULT_DEBUG_PORT))
    use_browser = bool(body.get("use_browser", False))
    browser_wait = float(body.get("browser_wait_seconds", 25.0))
    workers = max(1, min(int(body.get("workers", 2)), 10))

    task_id, task = _new_task()

    def _run():
        try:
            from skills.fulltext_acquisition import acquire_article_fulltext
            records = df.to_dict("records")
            total = len(records)
            mode = "HTTP + 浏览器 fallback" if use_browser else "HTTP"
            _push(task, f"开始全文获取（{total} 条，模式={mode}，并发={workers}）...")
            if total > 200:
                _push(
                    task,
                    f"[!! 警告] 本批次 {total} 条，超过经验安全阈值 200 条；"
                    "建议中断后分批跑，每批 ≤ 200，批与批之间间隔几分钟，"
                    "否则 Elsevier / Wiley 等可能识别为批量爬取并封禁机构 IP。",
                )
            if workers > 2:
                _push(
                    task,
                    f"[!! 警告] 并发数 {workers} 偏高（建议 1-2）；"
                    "每个域名后端已自动限流到 1 QPS，但跨域名仍可能触发风控。",
                )

            def _fetch_one(args: tuple[int, dict]) -> tuple[int, dict]:
                idx, record = args
                doi = str(record.get("DOI") or record.get("doi") or "").strip()
                if not doi:
                    record["acquisition_status"] = "skipped_no_doi"
                    _push(task, f"[{idx+1}/{total}] 无DOI，跳过")
                    return idx, record
                try:
                    if use_browser:
                        acq = acquire_article_fulltext_with_browser_fallback(
                            doi, debug_port=debug_port, timeout=timeout, browser_wait_seconds=browser_wait,
                        )
                    else:
                        acq = acquire_article_fulltext(doi, timeout=timeout)
                    record.update(acq)
                    status = acq.get("acquisition_status", "?")
                    acq_type = acq.get("acquisition_type", "")
                    _push(task, f"[{idx+1}/{total}] {doi[:50]} → {status} ({acq_type})")
                except Exception as exc:
                    record["acquisition_status"] = "error"
                    record["error_message"] = str(exc)[:300]
                    _push(task, f"[{idx+1}/{total}] {doi[:50]} → error")
                return idx, record

            result_map: dict[int, dict] = {}
            with ThreadPoolExecutor(max_workers=workers) as executor:
                futures = {executor.submit(_fetch_one, (i, rec)): i for i, rec in enumerate(records)}
                for future in as_completed(futures):
                    idx, record = future.result()
                    result_map[idx] = record

            result_records = [result_map[i] for i in range(total)]
            out_df = pd.DataFrame(result_records)
            payload, _packaged, result_format, fulltext_count = serialize_result_table(out_df)
            ok_count = sum(1 for r in result_records if r.get("acquisition_status") == "ok")
            _push(task, f"全文获取完成：{ok_count}/{total} 成功，{fulltext_count} 篇已保存全文，格式={result_format}")
            _finish(task, payload, result_format)
        except Exception as exc:
            import traceback
            _fail(task, f"{exc} | {traceback.format_exc()[-500:]}")

    threading.Thread(target=_run, daemon=True).start()
    return JSONResponse({"task_id": task_id})


# ── SSE / 任务状态 / 下载 ────────────────────────────────────────────────

@app.get("/api/progress/{task_id}")
async def progress(task_id: str):
    with _lock:
        task = _tasks.get(task_id)
    if task is None:
        raise HTTPException(404, "task not found")
    return StreamingResponse(_sse_generator(task), media_type="text/event-stream")


@app.get("/api/task/{task_id}")
async def task_status(task_id: str):
    with _lock:
        task = _tasks.get(task_id)
    if task is None:
        raise HTTPException(404, "task not found")
    return JSONResponse({
        "status": task["status"],
        "error": task.get("error", ""),
        "has_result": task.get("result") is not None,
        "result_format": task.get("result_format", ""),
        "wos_session_id": task.get("wos_session_id", ""),
        "screened_session_id": task.get("screened_session_id", ""),
        "row_count": task.get("row_count", 0),
        "added_count": task.get("added_count", 0),
        "source_row_count": task.get("source_row_count", 0),
    })


@app.get("/api/download/{task_id}")
async def download(task_id: str):
    with _lock:
        task = _tasks.get(task_id)
    if task is None or not task.get("result"):
        raise HTTPException(404, "结果未就绪")
    fmt = task.get("result_format", "")
    ext = "zip" if fmt == "zip" else "xlsx"
    media = "application/zip" if ext == "zip" else "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    return Response(task["result"], media_type=media,
                    headers={"Content-Disposition": f'attachment; filename="fetch_result.{ext}"'})


# ── 检索式生成 ────────────────────────────────────────────────────────────

@app.post("/api/generate-query")
async def generate_query(body: dict):
    topic = str(body.get("topic", "")).strip()
    model = str(body.get("model", "")).strip()
    base_url = str(body.get("base_url", "https://api.openai.com/v1")).strip() or "https://api.openai.com/v1"
    api_key = str(body.get("api_key", "")).strip()
    timeout = max(30, int(body.get("timeout", 120)))
    if not topic:
        raise HTTPException(400, "请输入研究主题")
    if not model or not api_key:
        raise HTTPException(400, "请配置 model 和 api_key")
    try:
        query = build_wos_search_query(
            topic_text=topic, model=model, base_url=base_url, api_key=api_key, timeout=timeout,
        )
    except Exception as exc:
        raise HTTPException(500, str(exc))
    return JSONResponse({"query": query})


@app.post("/api/generate-plan")
async def generate_plan(body: dict):
    topic = str(body.get("topic", "")).strip()
    model = str(body.get("model", "")).strip()
    base_url = str(body.get("base_url", "https://api.openai.com/v1")).strip() or "https://api.openai.com/v1"
    api_key = str(body.get("api_key", "")).strip()
    timeout = max(30, int(body.get("timeout", 180)))
    direction_count = str(body.get("direction_count", "auto")).strip() or "auto"
    if not topic:
        raise HTTPException(400, "请输入研究主题")
    if not model or not api_key:
        raise HTTPException(400, "请配置 model 和 api_key")
    try:
        plan = build_review_plan(
            topic_text=topic, model=model, base_url=base_url, api_key=api_key,
            timeout=timeout, direction_count=direction_count,
        )
    except Exception as exc:
        raise HTTPException(500, str(exc))
    return JSONResponse(plan)
