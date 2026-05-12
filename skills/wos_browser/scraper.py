from __future__ import annotations

import re
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Any
from urllib.parse import urlparse

from .cdp import CDPSession, evaluate_js, wait_for_condition, goto_url, dispatch_key_event, get_page_snapshot
from .browser import (
    DEFAULT_DEBUG_PORT,
    DOI_PATTERN,
    get_debug_targets,
    get_page_targets,
    open_debug_target,
    close_debug_target,
    find_wos_results_target,
)


def normalize_doi_candidate(value: Any) -> str:
    match = DOI_PATTERN.search(str(value or ""))
    if not match:
        return ""
    return match.group(0).rstrip(" .;,)]+")


def _string_value(value: Any) -> str:
    return str(value or "").strip()


def _first_nonblank(*values: Any) -> str:
    for value in values:
        text = _string_value(value)
        if text:
            return text
    return ""


def _split_page_range(value: Any) -> tuple[str, str]:
    text = str(value or "").strip()
    if not text:
        return "", ""
    match = re.search(r"([A-Za-z]?\d+)\s*[-–—]\s*([A-Za-z]?\d+)", text)
    if match:
        return match.group(1), match.group(2)
    match = re.search(r"\b([A-Za-z]?\d+)\b", text)
    if match:
        return match.group(1), ""
    return "", ""


def _looks_like_prose(value: str) -> bool:
    """True when a metadata field looks like prose / title bleed rather than a
    short volume/issue/page token."""
    text = str(value or "").strip()
    if not text:
        return False
    return len(text) > 24 or bool(re.search(r"\b[A-Za-z]{6,}\b", text))


def _normalize_wos_metadata(volume: str, issue: str, pages: str) -> tuple[str, str, str]:
    """The WoS results list sometimes flattens a run like ``Volume 56 Issue 12
    Pages 2002-2006`` into a single label-less string in the Volume cell while
    the Issue cell receives unrelated title-text bleed. Strip embedded
    ``Pages``/``Issue`` tokens out of the volume field and drop obvious
    title-text bleed from the issue field."""
    vol = str(volume or "").strip()
    iss = str(issue or "").strip()
    pgs = str(pages or "").strip()

    # Peel a trailing ``...Pages XYZ`` segment off the volume field. No \b
    # anchor: when the WoS DOM omits whitespace we get ``12Page2002-2006`` and
    # there is no word/non-word boundary between digits and letters.
    page_tail = re.search(r"Pages?\s*([A-Za-z0-9.\-]+)\s*$", vol, flags=re.IGNORECASE)
    if page_tail:
        candidate_pages = page_tail.group(1)
        vol = vol[: page_tail.start()].rstrip()
        if not pgs:
            pgs = candidate_pages

    # Peel a trailing ``...Issue XYZ`` segment off the volume field.
    issue_tail = re.search(r"Issue\s*([A-Za-z0-9.\-]+)\s*$", vol, flags=re.IGNORECASE)
    if issue_tail:
        candidate_issue = issue_tail.group(1)
        vol = vol[: issue_tail.start()].rstrip()
        if not iss or _looks_like_prose(iss):
            iss = candidate_issue

    if _looks_like_prose(iss):
        iss = ""

    return vol, iss, pgs


def _normalized_doi_candidates(values: Any) -> list[str]:
    items = values if isinstance(values, list) else [values]
    seen: set[str] = set()
    normalized_values: list[str] = []
    for value in items:
        normalized = normalize_doi_candidate(value)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        normalized_values.append(normalized)
    return normalized_values


def extract_ut_from_url(value: Any) -> str:
    text = str(value or "")
    match = re.search(r"(?:WOS|MEDLINE|BCI|CCC|DIIDW|ZOOREC|PPRN|PQDT):[A-Z0-9._-]+", text, flags=re.IGNORECASE)
    if match:
        return match.group(0).upper()
    match = re.search(r"/full-record/([^/?#]+)", text, flags=re.IGNORECASE)
    return match.group(1) if match else ""


def extract_records_from_html(html: str, page_url: str, page_index: int) -> list[dict[str, Any]]:
    block_pattern = re.compile(
        r"(?is)<(div|article|li)[^>]*>(?:(?!</(?:div|article|li)>).)*?(10\.\d{4,9}/[-._;()/:A-Z0-9]+).*?</(?:div|article|li)>"
    )
    title_pattern = re.compile(r"(?is)<a[^>]*>(.*?)</a>")
    year_pattern = re.compile(r"(19|20)\d{2}")
    tag_pattern = re.compile(r"<[^>]+>")

    seen: set[str] = set()
    records: list[dict[str, Any]] = []
    for match in block_pattern.finditer(html):
        block_html = match.group(0)
        doi = DOI_PATTERN.search(block_html)
        if not doi:
            continue
        doi_text = doi.group(0).rstrip(" .;,)")
        if doi_text in seen:
            continue
        seen.add(doi_text)

        title_match = title_pattern.search(block_html)
        title = ""
        if title_match:
            title = tag_pattern.sub(" ", title_match.group(1))
            title = re.sub(r"\s+", " ", title).strip()

        plain = tag_pattern.sub(" ", block_html)
        plain = re.sub(r"\s+", " ", plain).strip()
        plain_without_doi = DOI_PATTERN.sub(" ", plain)
        year_match = year_pattern.search(plain_without_doi)

        records.append(
            {
                "search_rank": len(records) + 1,
                "title": title or plain[:180],
                "doi": doi_text,
                "journal": "",
                "year": year_match.group(0) if year_match else "",
                "source_page_url": page_url,
                "source_page_index": page_index,
                "raw_snippet": plain[:400],
            }
        )

    if records:
        return records

    fallback_seen: set[str] = set()
    for doi in DOI_PATTERN.findall(html):
        doi_text = doi.rstrip(" .;,)")
        if doi_text in fallback_seen:
            continue
        fallback_seen.add(doi_text)
        records.append(
            {
                "search_rank": len(records) + 1,
                "title": "",
                "doi": doi_text,
                "journal": "",
                "year": "",
                "source_page_url": page_url,
                "source_page_index": page_index,
                "raw_snippet": "",
            }
        )
    return records


def collect_result_rows_via_dom(session: CDPSession, page_index: int) -> dict[str, Any]:
    js = """
(async () => {
  const pageIndex = %PAGE_INDEX%;
  const doiRegex = /10\\.\\d{4,9}\\/[\\-._;()/:A-Z0-9]+/ig;
  const clean = (value) => (value || '').replace(/\\s+/g, ' ').trim();
  const clip = (value, limit) => clean(value).slice(0, limit);
  const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));
  const bodyText = clean(document.body ? document.body.innerText : '');
  const totalResultsMatch = bodyText.match(/([\\d,]+)\\s+results\\s+from/i);
  const totalPagesMatch = bodyText.match(/Page\\s+\\d+\\s+of\\s+(\\d+)/i) || bodyText.match(/\\bof\\s+(\\d+)\\s+pages?\\b/i);
  const totalResults = totalResultsMatch ? Number((totalResultsMatch[1] || '').replace(/,/g, '')) : 0;
  const totalPages = totalPagesMatch ? Number(totalPagesMatch[1]) : 0;
  const inferredPerPage = totalResults && totalPages ? Math.max(1, Math.round(totalResults / totalPages)) : 50;
  const pageFromUrlMatch = location.pathname.match(/\\/(\\d+)$/);
  const currentPageNumber = pageFromUrlMatch ? Number(pageFromUrlMatch[1]) : pageIndex;
  const expectedRankMin = Math.max(1, ((currentPageNumber - 1) * inferredPerPage) + 1);
  const expectedRankMax = currentPageNumber * inferredPerPage;
  const hasPositiveResults = /[1-9][\\d,]*\\s+results\\s+from/i.test(bodyText) || /\\b[1-9][\\d,]*\\s+Documents\\b/i.test(bodyText);
  const noResults = !hasPositiveResults && (/\\b0\\s+results\\s+from\\b|\\b0\\s+Documents\\b|no results found|no records found|your search did not match/i.test(bodyText));
  if (noResults) {
    return {
      currentPageNumber,
      expectedRankMin,
      expectedRankMax,
      observedRecordCount: 0,
      maxRankSeen: 0,
      pageWideDoiCandidates: [],
      rows: [],
      noResults: true,
    };
  }

  const pushDoiMatches = (bucket, text) => {
    const found = String(text || '').match(doiRegex) || [];
    for (const item of found) {
      const normalized = item.replace(/[\\s.,;)+]+$/g, '');
      if (!normalized) continue;
      if (typeof bucket.add === 'function') bucket.add(normalized);
      else bucket.push(normalized);
    }
  };

  const hrefToDoi = (href) => {
    const matches = [];
    if (!href) return matches;
    pushDoiMatches(matches, href);
    try {
      const url = new URL(href, location.href);
      for (const [key, value] of url.searchParams.entries()) {
        const decoded = decodeURIComponent(value || '');
        if (/keyaid|desturl|doi|article/i.test(key)) pushDoiMatches(matches, decoded);
      }
    } catch (error) {}
    return matches;
  };

  const extractUtFromUrl = (value) => {
    const text = decodeURIComponent(String(value || ''));
    const match = text.match(/(?:WOS|MEDLINE|BCI|CCC|DIIDW|ZOOREC|PPRN|PQDT):[A-Z0-9._-]+/i);
    if (match) return match[0].toUpperCase();
    const fullRecordMatch = text.match(/\\/full-record\\/([^/?#]+)/i);
    return fullRecordMatch ? fullRecordMatch[1] : '';
  };

  const linesFromText = (text) => String(text || '').split(/\\n|\\r| {2,}/).map(clean).filter(Boolean);

  const labeledValue = (lines, labels) => {
    for (let index = 0; index < lines.length; index++) {
      const line = lines[index];
      for (const label of labels) {
        const pattern = new RegExp(`^${label}\\\\s*[:\\\\-]?\\\\s*(.+)$`, 'i');
        const match = line.match(pattern);
        if (match && clean(match[1])) return clean(match[1]);
        if (new RegExp(`^${label}$`, 'i').test(line) && lines[index + 1]) return clean(lines[index + 1]);
      }
    }
    return '';
  };

  const pageWideDoiSet = new Set();
  const pushPageWideMatches = () => {
    pushDoiMatches(pageWideDoiSet, document.body ? document.body.innerText : '');
    for (const link of Array.from(document.querySelectorAll('a[href]'))) {
      for (const doi of hrefToDoi(link.href)) pageWideDoiSet.add(doi);
      pushDoiMatches(pageWideDoiSet, [
        link.innerText,
        link.getAttribute('aria-label'),
        link.getAttribute('title'),
        link.getAttribute('data-ta'),
      ].filter(Boolean).join(' '));
    }
    for (const node of Array.from(document.querySelectorAll('[href*="10."], [content*="10."], [data-doi], [title*="10."], [aria-label*="10."]'))) {
      for (const attr of ['href', 'content', 'data-doi', 'title', 'aria-label']) {
        pushDoiMatches(pageWideDoiSet, node.getAttribute(attr));
      }
      pushDoiMatches(pageWideDoiSet, node.textContent || '');
    }
  };

  const getScrollableAreas = () => [
    document.scrollingElement || document.documentElement,
    ...Array.from(document.querySelectorAll('*')).filter((node) => {
      const style = window.getComputedStyle(node);
      return (
        node.scrollHeight > node.clientHeight + 80 &&
        /(auto|scroll)/i.test(`${style.overflowY} ${style.overflow}`)
      );
    }),
  ];

  const scrollScrollableAreas = () => {
    let moved = false;
    for (const node of getScrollableAreas().slice(0, 30)) {
      const before = node.scrollTop;
      const step = Math.max(700, Math.floor(node.clientHeight * 0.8));
      node.scrollTop = Math.min(node.scrollHeight, before + step);
      if (node.scrollTop !== before) moved = true;
    }
    return moved;
  };

  const canScrollMore = () => getScrollableAreas()
    .some((node) => node.scrollTop + node.clientHeight < node.scrollHeight - 20);

  const scrapeRenderedRecords = () => {
    const recordSelector = [
      'app-record',
      'app-summary-record',
      '[data-ta*="record" i]',
      '[class*="summary-record" i]',
      '[class*="record-card" i]',
    ].join(',');
    const cards = Array.from(document.querySelectorAll(recordSelector))
      .filter(card => card.querySelector('a[href*="/full-record/"], a[href*="full-record"]') || String(card.innerText || '').match(doiRegex));
    const rows = [];
    for (const card of cards) {
      const text = clean(card.innerText || '');
      const lines = linesFromText(card.innerText || '');
      const rankMatch = text.match(/^(\\d+)\\b/);
      const rank = rankMatch ? Number(rankMatch[1]) : 0;
      const titleNode = card.querySelector('a[href*="/full-record/"], a[href*="full-record"]');
      const title = clip(titleNode ? titleNode.innerText : '', 500);
      const fullRecordLink = titleNode ? titleNode.href : '';
      const utUniqueId = clip(extractUtFromUrl(fullRecordLink) || labeledValue(lines, ['UT \\\\(Unique WOS ID\\\\)', 'UT \\\\(Unique ID\\\\)', 'Accession Number', 'Unique ID', 'UT']), 120);
      const sourceTitle = clip(labeledValue(lines, ['Source Title', 'Source', 'Journal', 'Publication Title', 'Publication Name']), 500);
      const authors = clip(labeledValue(lines, ['Authors', 'Author(s)', 'By', 'Byline']), 1200);
      const volume = clip(labeledValue(lines, ['Volume', 'Vol']), 80);
      const issue = clip(labeledValue(lines, ['Issue', 'Number', 'No']), 80);
      const pages = clip(labeledValue(lines, ['Pages', 'Page', 'Article Number', 'Early Access']), 120);
      const documentType = clip(labeledValue(lines, ['Document Type', 'Document Types', 'Publication Type']), 200);
      const doiMatches = [];
      pushDoiMatches(doiMatches, text);
      for (const link of Array.from(card.querySelectorAll('a[href]'))) {
        for (const doi of hrefToDoi(link.href)) doiMatches.push(doi);
      }
      const labeledYear = labeledValue(lines, ['Publication Year', 'Published', 'Year', 'PY']);
      const yearMatch = String(labeledYear || text.replace(doiRegex, ' ')).match(/(19|20)\\d{2}/);
      rows.push({
        rank,
        title,
        articleTitle: title,
        sourceTitle,
        authors,
        volume,
        issue,
        pages,
        documentType,
        publicationYear: yearMatch ? yearMatch[0] : '',
        utUniqueId,
        fullRecordLink,
        doiCandidates: Array.from(new Set(doiMatches)).slice(0, 10),
        year: yearMatch ? yearMatch[0] : '',
        raw_snippet: text.slice(0, 240),
      });
    }
    return rows;
  };

  const recordsByFullRecord = new Map();
  let stableCount = 0;
  let previousCount = -1;
  let previousPageWideCount = -1;
  let maxRankSeen = 0;
  let previousMaxRankSeen = 0;
  let stableMaxRankCount = 0;
  for (let step = 0; step < 40; step++) {
    pushPageWideMatches();
    const rendered = scrapeRenderedRecords();
    for (const row of rendered) {
      if (!row.fullRecordLink && !(row.doiCandidates || []).length) continue;
      if (row.rank) maxRankSeen = Math.max(maxRankSeen, row.rank);
      const key = row.fullRecordLink || `doi:${(row.doiCandidates || []).join('|')}`;
      if (!recordsByFullRecord.has(key)) {
        recordsByFullRecord.set(key, row);
      } else {
        const existing = recordsByFullRecord.get(key);
        existing.doiCandidates = Array.from(new Set([...(existing.doiCandidates || []), ...(row.doiCandidates || [])])).slice(0, 10);
        if (!existing.year && row.year) existing.year = row.year;
        if (!existing.articleTitle && row.articleTitle) existing.articleTitle = row.articleTitle;
        if (!existing.sourceTitle && row.sourceTitle) existing.sourceTitle = row.sourceTitle;
        if (!existing.authors && row.authors) existing.authors = row.authors;
        if (!existing.volume && row.volume) existing.volume = row.volume;
        if (!existing.issue && row.issue) existing.issue = row.issue;
        if (!existing.pages && row.pages) existing.pages = row.pages;
        if (!existing.documentType && row.documentType) existing.documentType = row.documentType;
        if (!existing.publicationYear && row.publicationYear) existing.publicationYear = row.publicationYear;
        if (!existing.utUniqueId && row.utUniqueId) existing.utUniqueId = row.utUniqueId;
        if (!existing.raw_snippet && row.raw_snippet) existing.raw_snippet = row.raw_snippet;
      }
    }

    const currentCount = recordsByFullRecord.size;
    if (currentCount === previousCount) stableCount += 1;
    else stableCount = 0;
    previousCount = currentCount;

    if (maxRankSeen === previousMaxRankSeen) stableMaxRankCount += 1;
    else stableMaxRankCount = 0;
    previousMaxRankSeen = maxRankSeen;

    const pageWideCount = pageWideDoiSet.size;
    const pageWideStable = pageWideCount === previousPageWideCount;
    previousPageWideCount = pageWideCount;

    if (currentCount > 0 && step >= 8 && stableCount >= 4 && stableMaxRankCount >= 4 && pageWideStable && !canScrollMore()) break;

    const moved = scrollScrollableAreas();
    if (!moved && currentCount > 0 && pageWideStable && step >= 5) break;
    await sleep(500);
  }
  pushPageWideMatches();

  const rows = Array.from(recordsByFullRecord.values())
    .sort((a, b) => (a.rank || 0) - (b.rank || 0));

  return {
    currentPageNumber,
    expectedRankMin,
    expectedRankMax,
    observedRecordCount: rows.length,
    maxRankSeen,
    pageWideDoiCandidates: Array.from(pageWideDoiSet).slice(0, 80),
    rows,
  };
})()
""".replace("%PAGE_INDEX%", str(page_index))
    result = evaluate_js(session, js, await_promise=True)
    return result if isinstance(result, dict) else {"rows": []}


def wait_for_results_page_loaded(session: CDPSession, timeout_seconds: float = 30.0) -> None:
    wait_for_condition(
        session,
        """
(() => {
  const text = String(document.body ? document.body.innerText : '');
  const fullRecordLinks = document.querySelectorAll('a[href*="/full-record/"], a[href*="full-record"]').length;
  const recordNodes = document.querySelectorAll('app-record, app-summary-record, [class*="summary-record" i], [class*="record-card" i]').length;
  const loadingText = /loading|please wait/i.test(text);
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i]'))
    .some(node => node.offsetParent !== null || node.getClientRects().length);
  const hasPositiveResults = /[1-9][\\d,]*\\s+results\\s+from/i.test(text) || /\\b[1-9][\\d,]*\\s+Documents\\b/i.test(text);
  const noResults = !hasPositiveResults && (/\\b0\\s+results\\s+from\\b|\\b0\\s+Documents\\b|no results found|no records found|your search did not match/i.test(text));
  return text.length > 500 && (fullRecordLinks > 0 || recordNodes > 0 || noResults) && !loadingText && !loadingNode;
})()
""",
        timeout_seconds=timeout_seconds,
        poll_interval=0.25,
    )


def wait_for_full_record_page_ready(session: CDPSession, timeout_seconds: float = 30.0) -> dict[str, Any]:
    deadline = time.time() + max(3.0, float(timeout_seconds or 30.0))
    snapshot: dict[str, Any] = {}
    while time.time() < deadline:
        try:
            current = evaluate_js(
                session,
                """
(() => {
  const text = String(document.body ? document.body.innerText : '');
  const url = String(location.href || '');
  const title = String(document.title || '');
  const loadingText = /loading|please wait|正在加载|请稍候/i.test(text.slice(0, 1600));
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i], [class*="spinner" i]'))
    .some(node => node.offsetParent !== null || node.getClientRects().length);
  const hasFullRecordUrl = url.includes('/full-record/');
  const hasDoi = /10\\.\\d{4,9}\\/[\\-._;()/:A-Z0-9]+/i.test(text) || document.querySelectorAll('[data-ta="FullRTa-DOI"], .doi-link, a[href*="doi.org/"]').length > 0;
  const hasTitle = !!document.querySelector('[data-ta*="FullRTa-title" i], [data-ta*="title" i] h2, h1, h2') || title.length > 8;
  const hasRecordSignal = /\\b(Source|Abstract|Author Keywords|Keywords Plus|Document Type|Document Information|Accession Number|UT \\(|Times Cited|Cited References|Publication Year|Volume|Issue|Pages)\\b/i.test(text);
  const enoughInfo = (hasTitle && hasRecordSignal) || text.length > 2500;
  return {
    url,
    title,
    textChars: text.length,
    hasFullRecordUrl,
    hasDoi,
    hasTitle,
    hasRecordSignal,
    enoughInfo,
    loading: loadingText || loadingNode,
  };
})()
""",
            )
        except Exception:
            time.sleep(0.25)
            continue
        snapshot = current if isinstance(current, dict) else {}
        has_doi = bool(snapshot.get("hasDoi"))
        text_chars = int(snapshot.get("textChars") or 0)
        ready = (
            bool(snapshot.get("hasFullRecordUrl"))
            and text_chars >= 900
            and not bool(snapshot.get("loading"))
            and bool(snapshot.get("enoughInfo"))
        )
        if ready:
            return snapshot
        time.sleep(0.25)
    return snapshot


def extract_doi_from_full_record_page(full_record_url: str, port: int = DEFAULT_DEBUG_PORT) -> dict[str, Any]:
    target = open_debug_target(full_record_url, port=port)
    ws_url = target.get("webSocketDebuggerUrl")
    if not ws_url:
        close_debug_target(str(target.get("id", "")), port=port)
        raise ValueError("The temporary full-record target does not expose a debugger websocket.")

    session = CDPSession(ws_url)
    try:
        session.call("Page.enable")
        session.call("Runtime.enable")
        try:
            current_url = str(evaluate_js(session, "location.href") or "")
        except Exception:
            current_url = ""
        if current_url in {"", "about:blank"} or "/full-record/" not in current_url:
            try:
                session.call("Page.navigate", {"url": full_record_url})
            except Exception:
                goto_url(session, full_record_url)

        ready_snapshot = wait_for_full_record_page_ready(session, timeout_seconds=30.0)
        if not (
            bool(ready_snapshot.get("hasFullRecordUrl"))
            and int(ready_snapshot.get("textChars") or 0) >= 450
            and bool(ready_snapshot.get("enoughInfo"))
        ):
            current_url = str(evaluate_js(session, "location.href") or "")
            title = str(evaluate_js(session, "document.title") or "")
            text_chars = int(evaluate_js(session, "document.body ? document.body.innerText.length : 0") or 0)
            raise ValueError(
                "full-record page did not become ready: "
                f"url={current_url or 'about:blank'} title={title} text_chars={text_chars} "
                f"last_snapshot={ready_snapshot}"
            )
        js = """
(() => {
  const doiRegex = /10\\.\\d{4,9}\\/[\\-._;()/:A-Z0-9]+/ig;
  const clean = (value) => (value || '').replace(/\\s+/g, ' ').trim();
  const clip = (value, limit) => clean(value).slice(0, limit);
  const pushMatches = (bucket, text) => {
    const found = String(text || '').match(doiRegex) || [];
    for (const item of found) {
      const normalized = item.replace(/[\\s.,;)+]+$/g, '');
      if (normalized) bucket.push(normalized);
    }
  };
  const hrefToDoi = (href) => {
    const matches = [];
    if (!href) return matches;
    pushMatches(matches, href);
    try {
      const url = new URL(href, location.href);
      for (const [key, value] of url.searchParams.entries()) {
        const decoded = decodeURIComponent(value || '');
        if (/keyaid|desturl|doi|article/i.test(key)) pushMatches(matches, decoded);
      }
    } catch (error) {}
    return matches;
  };

  const extractUtFromUrl = (value) => {
    const text = decodeURIComponent(String(value || ''));
    const match = text.match(/(?:WOS|MEDLINE|BCI|CCC|DIIDW|ZOOREC|PPRN|PQDT):[A-Z0-9._-]+/i);
    if (match) return match[0].toUpperCase();
    const fullRecordMatch = text.match(/\\/full-record\\/([^/?#]+)/i);
    return fullRecordMatch ? fullRecordMatch[1] : '';
  };

  const linesFromText = (text) => String(text || '').split(/\\n|\\r| {2,}/).map(clean).filter(Boolean);

  const labeledValue = (lines, labels) => {
    for (let index = 0; index < lines.length; index++) {
      const line = lines[index];
      for (const label of labels) {
        const pattern = new RegExp(`^${label}\\\\s*[:\\\\-]?\\\\s*(.+)$`, 'i');
        const match = line.match(pattern);
        if (match && clean(match[1])) return clean(match[1]);
        if (new RegExp(`^${label}$`, 'i').test(line) && lines[index + 1]) return clean(lines[index + 1]);
      }
    }
    return '';
  };

  const firstText = (selectors) => {
    for (const selector of selectors) {
      const node = document.querySelector(selector);
      const text = clean(node ? node.innerText || node.textContent || '' : '');
      if (text) return text;
    }
    return '';
  };

  const sectionValue = (labels, stopLabels) => {
    for (let index = 0; index < lines.length; index++) {
      const line = lines[index];
      const matched = labels.some((label) => new RegExp(`^${label}\\\\s*[:\\\\-]?\\\\s*(.*)$`, 'i').test(line));
      if (!matched) continue;
      const inline = labels
        .map((label) => line.match(new RegExp(`^${label}\\\\s*[:\\\\-]?\\\\s*(.+)$`, 'i')))
        .find(Boolean);
      if (inline && clean(inline[1])) return clean(inline[1]);
      const values = [];
      for (let cursor = index + 1; cursor < lines.length; cursor++) {
        const next = lines[cursor];
        if (stopLabels.some((label) => new RegExp(`^${label}\\\\s*[:\\\\-]?`, 'i').test(next))) break;
        values.push(next);
        if (values.join(' ').length > 2500) break;
      }
      return clean(values.join(' '));
    }
    return '';
  };

  const candidates = [];
  const bodyText = clean(document.body ? document.body.innerText : '');
  const lines = linesFromText(document.body ? document.body.innerText : '');
  const currentItemId = location.pathname.split('/').pop() || '';
  const topSectionText = bodyText.split('Abstract')[0].slice(0, 4000);
  pushMatches(candidates, topSectionText);

  const doiFieldNodes = Array.from(document.querySelectorAll('[data-ta="FullRTa-DOI"], .doi-link, a[href*="doi.org/"]'));
  for (const node of doiFieldNodes) {
    pushMatches(candidates, node.innerText || '');
    if (node.href) {
      for (const doi of hrefToDoi(node.href)) candidates.push(doi);
    }
  }

  const fullTextLinks = Array.from(document.querySelectorAll('a[href]')).filter((link) => {
    const text = [link.innerText, link.getAttribute('aria-label'), link.getAttribute('data-ta'), link.className]
      .filter(Boolean)
      .join(' ')
      .toLowerCase();
    const href = String(link.href || '');
    return (
      text.includes('full text') ||
      text.includes('publisher') ||
      text.includes('view full text') ||
      text.includes('frlinkta-link') ||
      text.includes('full-record-links') ||
      (currentItemId && href.includes(`SrcItemId=${currentItemId}`))
    );
  });
  for (const link of fullTextLinks) {
    for (const doi of hrefToDoi(link.href)) candidates.push(doi);
  }
  const timesCitedText = clip(
    labeledValue(lines, ['Times Cited', 'Times Cited, All Databases', 'Times Cited, WoS Core', 'Times Cited Count']) ||
    sectionValue(['Times Cited', 'Times Cited, All Databases'], ['Cited References', 'Related Records', 'Citation Network', 'Abstract']),
    120
  );
  const citedReferencesText = clip(
    sectionValue(['Cited References', 'References'], ['Related Records', 'Times Cited', 'Citation Network', 'Abstract', 'Author Keywords', 'Keywords Plus']) ||
    labeledValue(lines, ['Cited References', 'References']),
    4000
  );
  const citedReferencesCount = clip(
    (citedReferencesText.match(/\\b\\d{1,6}\\b/) || [''])[0] ||
    (labeledValue(lines, ['Cited References', 'References']).match(/\\b\\d{1,6}\\b/) || [''])[0],
    40
  );

  return {
    title: clip(firstText(['[data-ta*="FullRTa-title" i]', '[data-ta*="title" i] h2', 'h2', 'h1']) || clean(document.title), 500),
    sourceTitle: clip(firstText(['[data-ta*="SourceTitle" i]', '[data-ta*="source-title" i]']) || labeledValue(lines, ['Source Title', 'Source', 'Journal', 'Publication Title', 'Publication Name']), 500),
    authors: clip(firstText(['[data-ta*="author" i]', '[class*="author" i]']) || labeledValue(lines, ['Authors', 'Author(s)', 'By', 'Byline']), 1200),
    publicationYear: (labeledValue(lines, ['Publication Year', 'Published', 'Year', 'PY']).match(/(19|20)\\d{2}/) || [''])[0],
    volume: clip(labeledValue(lines, ['Volume', 'Vol']), 80),
    issue: clip(labeledValue(lines, ['Issue', 'Number', 'No']), 80),
    pages: clip(labeledValue(lines, ['Pages', 'Page', 'Article Number', 'Early Access']), 120),
    documentType: clip(labeledValue(lines, ['Document Type', 'Document Types', 'Publication Type']), 200),
    abstractText: clip(sectionValue(['Abstract'], ['Author Keywords', 'Keywords Plus', 'Addresses', 'Categories', 'Document Information', 'Funding', 'References']), 3000),
    authorKeywords: clip(labeledValue(lines, ['Author Keywords', 'Keywords']) || sectionValue(['Author Keywords'], ['Keywords Plus', 'Abstract', 'Addresses', 'Categories', 'Document Information', 'Funding', 'References']), 1200),
    keywordsPlus: clip(labeledValue(lines, ['Keywords Plus']) || sectionValue(['Keywords Plus'], ['Author Keywords', 'Abstract', 'Addresses', 'Categories', 'Document Information', 'Funding', 'References']), 1200),
    timesCited: timesCitedText,
    citedReferencesCount,
    citedReferencesText,
    utUniqueId: clip(extractUtFromUrl(location.href) || labeledValue(lines, ['UT \\\\(Unique WOS ID\\\\)', 'UT \\\\(Unique ID\\\\)', 'Accession Number', 'Unique ID', 'UT']), 120),
    url: location.href,
    doiCandidates: Array.from(new Set(candidates)).slice(0, 10),
    textChars: document.body ? document.body.innerText.length : 0,
  };
})()
"""
        result = evaluate_js(session, js)
        return result if isinstance(result, dict) else {"doiCandidates": []}
    finally:
        session.close()
        close_debug_target(str(target.get("id", "")), port=port)


def extract_records_via_dom(
    session: CDPSession,
    page_index: int,
    port: int = DEFAULT_DEBUG_PORT,
    detail_workers: int = 2,
    max_records: int = 0,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    try:
        payload = collect_result_rows_via_dom(session, page_index)
    except Exception as exc:
        return [], {
            "page_index": page_index,
            "current_page_number": page_index,
            "expected_rank_min": 0,
            "expected_rank_max": 0,
            "records_seen": 0,
            "doi_found": 0,
            "page_wide_doi_found": 0,
            "page_wide_doi_recovered": 0,
            "detail_workers": 0,
            "detail_pages_opened": 0,
            "detail_errors": [],
            "without_doi": 0,
            "without_doi_ranks": [],
            "without_doi_rows": [],
            "no_results": False,
            "dom_error": str(exc)[:500],
        }
    rows = payload.get("rows", [])
    if not isinstance(rows, list):
        rows = []
    page_wide_doi_candidates = payload.get("pageWideDoiCandidates", [])
    if not isinstance(page_wide_doi_candidates, list):
        page_wide_doi_candidates = []

    prepared_rows: list[dict[str, Any]] = []
    detail_rows: list[tuple[int, dict[str, Any]]] = []
    limit = max(0, int(max_records or 0))
    for row in rows:
        if limit and len(prepared_rows) >= limit:
            break
        if not isinstance(row, dict):
            continue
        full_record_link = str(row.get("fullRecordLink", "") or "").strip()
        row_doi_candidates = _normalized_doi_candidates(row.get("doiCandidates", []))
        if not full_record_link and not row_doi_candidates:
            continue
        row_index = len(prepared_rows)
        prepared_rows.append(row)
        if full_record_link:
            detail_rows.append((row_index, row))

    def fetch_detail(row: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], str]:
        full_record_link = str(row.get("fullRecordLink", "") or "")
        try:
            fallback = extract_doi_from_full_record_page(full_record_link, port=port)
            return row, fallback, ""
        except Exception as exc:
            return row, {}, str(exc)

    detail_results_by_index: dict[int, tuple[dict[str, Any], str]] = {}
    worker_count = max(1, min(int(detail_workers or 1), 8))
    if detail_rows:
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = {executor.submit(fetch_detail, row): row_index for row_index, row in detail_rows}
            for future in as_completed(futures):
                _, fallback, error = future.result()
                detail_results_by_index[futures[future]] = (fallback, error)

    rows_without_doi: list[dict[str, Any]] = []
    missing_rows: list[tuple[dict[str, Any], dict[str, Any], str]] = []
    final_rows: list[dict[str, Any]] = []
    seen_doi: set[str] = set()
    detail_errors: list[dict[str, Any]] = []
    recovered_from_page_scan = 0

    def append_final_row(
        row: dict[str, Any],
        fallback: dict[str, Any],
        doi_candidates: list[str],
        recovery_source: str = "",
    ) -> bool:
        full_record_link = str(row.get("fullRecordLink", "") or "")
        article_title = _first_nonblank(fallback.get("title"), row.get("articleTitle"), row.get("title"))
        source_title = _first_nonblank(fallback.get("sourceTitle"), row.get("sourceTitle"))
        authors = _first_nonblank(fallback.get("authors"), row.get("authors"))
        publication_year = _first_nonblank(fallback.get("publicationYear"), row.get("publicationYear"), row.get("year"))
        volume = _first_nonblank(fallback.get("volume"), row.get("volume"))
        issue = _first_nonblank(fallback.get("issue"), row.get("issue"))
        pages = _first_nonblank(fallback.get("pages"), row.get("pages"))
        volume, issue, pages = _normalize_wos_metadata(volume, issue, pages)
        start_page, end_page = _split_page_range(pages)
        document_type = _first_nonblank(fallback.get("documentType"), row.get("documentType"))
        author_keywords = _first_nonblank(fallback.get("authorKeywords"), row.get("authorKeywords"))
        keywords_plus = _first_nonblank(fallback.get("keywordsPlus"), row.get("keywordsPlus"))
        abstract_text = _first_nonblank(fallback.get("abstractText"), row.get("abstractText"))
        times_cited = _first_nonblank(fallback.get("timesCited"), row.get("timesCited"))
        cited_references_count = _first_nonblank(fallback.get("citedReferencesCount"), row.get("citedReferencesCount"))
        cited_references_text = _first_nonblank(fallback.get("citedReferencesText"), row.get("citedReferencesText"))
        ut_unique_id = _first_nonblank(fallback.get("utUniqueId"), row.get("utUniqueId"), extract_ut_from_url(full_record_link))

        normalized = str(doi_candidates[0] or "").strip()
        if not normalized or normalized in seen_doi:
            return False
        seen_doi.add(normalized)
        output_row = {
            "search_rank": len(final_rows) + 1,
            "rank": row.get("rank", 0),
            "Article Title": article_title,
            "Authors": authors,
            "Source Title": source_title,
            "DOI": normalized,
            "Publication Year": publication_year,
            "Volume": volume,
            "Issue": issue,
            "Pages": pages,
            "Start Page": start_page,
            "End Page": end_page,
            "Document Type": document_type,
            "Abstract": abstract_text,
            "Author Keywords": author_keywords,
            "Keywords Plus": keywords_plus,
            "Times Cited": times_cited,
            "Cited References Count": cited_references_count,
            "Cited References": cited_references_text,
            "UT (Unique ID)": ut_unique_id,
            "article_title": article_title,
            "authors": authors,
            "source_title": source_title,
            "publication_year": publication_year,
            "volume": volume,
            "issue": issue,
            "pages": pages,
            "start_page": start_page,
            "end_page": end_page,
            "document_type": document_type,
            "abstract": abstract_text,
            "author_keywords": author_keywords,
            "keywords_plus": keywords_plus,
            "times_cited": times_cited,
            "cited_references_count": cited_references_count,
            "cited_references": cited_references_text,
            "ut_unique_id": ut_unique_id,
            "title": article_title,
            "doi": f"https://doi.org/{normalized}",
            "doi_raw": normalized,
            "journal": source_title,
            "year": publication_year,
            "source_page_index": page_index,
            "raw_snippet": row.get("raw_snippet", ""),
            "full_record_link": full_record_link,
            "doi_candidates": "; ".join(doi_candidates),
        }
        if recovery_source:
            output_row["doi_recovery_source"] = recovery_source
        final_rows.append(output_row)
        return True

    for index, row in enumerate(prepared_rows):
        fallback, error = detail_results_by_index.get(index, ({}, ""))
        full_record_link = str(row.get("fullRecordLink", "") or "")
        if error:
            detail_errors.append({"rank": row.get("rank", 0), "full_record_link": full_record_link, "error": error})

        row_candidates = _normalized_doi_candidates(row.get("doiCandidates", []))
        fallback_candidates = _normalized_doi_candidates(fallback.get("doiCandidates", []))
        doi_candidates = list(dict.fromkeys([*row_candidates, *fallback_candidates]))

        if doi_candidates:
            append_final_row(row, fallback, doi_candidates)
            continue

        article_title = _first_nonblank(fallback.get("title"), row.get("articleTitle"), row.get("title"))
        source_title = _first_nonblank(fallback.get("sourceTitle"), row.get("sourceTitle"))
        authors = _first_nonblank(fallback.get("authors"), row.get("authors"))
        publication_year = _first_nonblank(fallback.get("publicationYear"), row.get("publicationYear"), row.get("year"))
        ut_unique_id = _first_nonblank(fallback.get("utUniqueId"), row.get("utUniqueId"), extract_ut_from_url(full_record_link))
        missing_rows.append((row, fallback, error))
        rows_without_doi.append(
            {
                "rank": row.get("rank", 0),
                "title": article_title,
                "authors": authors,
                "source_title": source_title,
                "publication_year": publication_year,
                "ut_unique_id": ut_unique_id,
                "full_record_link": full_record_link,
                "error": error,
            }
        )

    page_wide_unused = [
        doi for doi in _normalized_doi_candidates(page_wide_doi_candidates) if doi not in seen_doi
    ]
    if len(missing_rows) == 1 and len(page_wide_unused) == 1:
        row, fallback, _ = missing_rows[0]
        if append_final_row(row, fallback, [page_wide_unused[0]], recovery_source="page_wide_single_unused"):
            recovered_from_page_scan = 1
            rows_without_doi = []

    diagnostics = {
        "page_index": page_index,
        "current_page_number": payload.get("currentPageNumber", page_index),
        "expected_rank_min": payload.get("expectedRankMin", 0),
        "expected_rank_max": payload.get("expectedRankMax", 0),
        "records_seen": len(prepared_rows),
        "doi_found": len(final_rows),
        "page_wide_doi_found": len({normalize_doi_candidate(value) for value in page_wide_doi_candidates if normalize_doi_candidate(value)}),
        "page_wide_doi_recovered": recovered_from_page_scan,
        "detail_workers": worker_count,
        "detail_pages_opened": len(detail_rows),
        "detail_errors": detail_errors[:20],
        "without_doi": len(rows_without_doi),
        "without_doi_ranks": [row.get("rank", 0) for row in rows_without_doi if row.get("rank")],
        "without_doi_rows": rows_without_doi,
        "no_results": bool(payload.get("noResults")),
    }
    return final_rows, diagnostics


def click_next_page(session: CDPSession) -> bool:
    js = """
(() => {
  const textMatch = (value) => /next|next page|›|»/i.test((value || '').trim());
  const candidates = Array.from(document.querySelectorAll('button, a, [role="button"]'));
  for (const node of candidates) {
    const label = [
      node.innerText,
      node.getAttribute('aria-label'),
      node.getAttribute('title'),
      node.getAttribute('data-ta')
    ].filter(Boolean).join(' ');
    const disabled = node.disabled || node.getAttribute('aria-disabled') === 'true' || String(node.className).includes('disabled');
    if (!disabled && textMatch(label)) {
      node.click();
      return true;
    }
  }
  return false;
})()
"""
    return bool(evaluate_js(session, js))


def build_next_page_url(current_url: str) -> str:
    parsed = urlparse(current_url)
    path = parsed.path
    match = re.search(r"^(.*?/summary/[^/]+/[^/]+/)(\d+)$", path)
    if not match:
        return ""
    prefix, page_str = match.group(1), match.group(2)
    next_page = int(page_str) + 1
    return f"{parsed.scheme}://{parsed.netloc}{prefix}{next_page}"


def trigger_zotero_save_shortcut(session: CDPSession) -> None:
    try:
        evaluate_js(session, "window.focus(); true;")
    except Exception:
        pass

    dispatch_key_event(session, event_type="rawKeyDown", key="Control", code="ControlLeft", windows_virtual_key_code=17, modifiers=2)
    dispatch_key_event(session, event_type="rawKeyDown", key="Shift", code="ShiftLeft", windows_virtual_key_code=16, modifiers=10)
    dispatch_key_event(session, event_type="keyDown", key="S", code="KeyS", windows_virtual_key_code=83, modifiers=10)
    dispatch_key_event(session, event_type="keyUp", key="S", code="KeyS", windows_virtual_key_code=83, modifiers=10)
    dispatch_key_event(session, event_type="keyUp", key="Shift", code="ShiftLeft", windows_virtual_key_code=16, modifiers=2)
    dispatch_key_event(session, event_type="keyUp", key="Control", code="ControlLeft", windows_virtual_key_code=17, modifiers=0)


def scrape_wos_pages(
    port: int = DEFAULT_DEBUG_PORT,
    max_pages: int = 5,
    wait_seconds: float = 30.0,
    detail_workers: int = 2,
    max_records: int = 0,
) -> dict[str, Any]:
    target = find_wos_results_target(port)
    ws_url = target.get("webSocketDebuggerUrl")
    if not ws_url:
        raise ValueError("The active browser target does not expose a debugger websocket.")

    session = CDPSession(ws_url)
    try:
        session.call("Page.enable")
        session.call("Runtime.enable")

        all_records: list[dict[str, Any]] = []
        seen_record_keys: set[str] = set()
        page_diagnostics: list[dict[str, Any]] = []
        page_count = 0
        previous_url = ""

        for page_index in range(1, max_pages + 1):
            remaining_records = max(0, int(max_records or 0) - len(all_records)) if max_records else 0
            if max_records and remaining_records <= 0:
                break
            snapshot = get_page_snapshot(session)
            page_url = str(snapshot.get("url", ""))
            html = str(snapshot.get("html", ""))
            text = str(snapshot.get("text", ""))
            if "webofscience" not in page_url.lower():
                raise ValueError("The active page does not look like a Web of Science page.")
            wait_for_results_page_loaded(session)
            snapshot = get_page_snapshot(session)
            page_url = str(snapshot.get("url", ""))
            html = str(snapshot.get("html", ""))
            text = str(snapshot.get("text", ""))

            page_records, diagnostics = extract_records_via_dom(
                session,
                page_index,
                port=port,
                detail_workers=detail_workers,
                max_records=remaining_records,
            )
            if not page_records:
                no_results = bool(diagnostics.get("no_results"))
                page_records = extract_records_from_html(html, page_url, page_index)
                diagnostics = {
                    "page_index": page_index,
                    "current_page_number": page_index,
                    "expected_rank_min": 0,
                    "expected_rank_max": 0,
                    "records_seen": 0,
                    "doi_found": len(page_records),
                    "without_doi": 0,
                    "without_doi_ranks": [],
                    "without_doi_rows": [],
                    "no_results": no_results,
                    "fallback_mode": "html_regex",
                }
            for record in page_records:
                doi = normalize_doi_candidate(record.get("DOI") or record.get("doi_raw") or record.get("doi") or "")
                ut = _string_value(record.get("UT (Unique ID)") or record.get("ut_unique_id")).upper()
                full_record_link = _string_value(record.get("full_record_link")).rstrip("/")
                key = (f"doi:{doi.lower()}" if doi else "") or (f"ut:{ut}" if ut else "") or (f"url:{full_record_link}" if full_record_link else "")
                if not key or key in seen_record_keys:
                    continue
                seen_record_keys.add(key)
                if doi:
                    record["DOI"] = doi
                    record["doi_raw"] = doi
                    record["doi"] = f"https://doi.org/{doi}"
                record["source_page_url"] = page_url
                all_records.append(record)
                if max_records and len(all_records) >= int(max_records or 0):
                    break

            diagnostics["source_page_url"] = page_url
            diagnostics["title"] = str(snapshot.get("title", ""))
            page_diagnostics.append(diagnostics)
            page_count += 1
            if page_index >= max_pages:
                break
            if max_records and len(all_records) >= int(max_records or 0):
                break
            if diagnostics.get("no_results"):
                break

            next_url = build_next_page_url(page_url)
            if next_url:
                if not goto_url(session, next_url):
                    break
            elif not click_next_page(session):
                break

            # Wait for the URL to actually change before checking page load state.
            # Without this, wait_for_results_page_loaded may see the already-loaded
            # current page and return immediately, causing the next iteration to
            # scrape duplicate content instead of the new page.
            _nav_deadline = time.time() + 8.0
            while time.time() < _nav_deadline:
                try:
                    _new_url = str(evaluate_js(session, "location.href") or "")
                    if _new_url and _new_url != page_url:
                        break
                except Exception:
                    pass
                time.sleep(0.25)

            wait_for_results_page_loaded(session, timeout_seconds=max(10.0, float(wait_seconds or 30.0)))

        return {
            "records": all_records,
            "pages_scraped": page_count,
            "page_diagnostics": page_diagnostics,
            "records_seen_total": sum(int(item.get("records_seen", 0) or 0) for item in page_diagnostics),
            "without_doi_total": sum(int(item.get("without_doi", 0) or 0) for item in page_diagnostics),
            "target_title": target.get("title", ""),
            "target_url": target.get("url", ""),
            "last_page_text_chars": int(snapshot.get("text_chars", len(text)) or 0),
            "last_page_html_chars": int(snapshot.get("html_chars", len(html)) or 0),
        }
    finally:
        session.close()
