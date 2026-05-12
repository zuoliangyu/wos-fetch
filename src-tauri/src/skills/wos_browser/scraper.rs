//! Scrape the WoS results page(s) for records, recovering DOIs from full-record
//! pages when the summary row doesn't expose one.
//!
//! Port of `skills/wos_browser/scraper.py`. The JS payloads (the big DOM
//! walker plus the full-record extractor) are preserved verbatim — they have
//! been hardened against multiple WoS UI revisions and rewriting them in Rust
//! would lose that accumulated tuning.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;
use tokio::time::{sleep, Instant};
use url::Url;

use super::cdp::{
    evaluate_js, evaluate_js_with, get_page_snapshot, goto_url, prepare_page_session,
    wait_for_condition, CdpSession,
};
use super::tools::{close_debug_target, find_wos_results_target, open_debug_target};
use crate::{AppError, AppResult};

static DOI_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)10\.\d{4,9}/[-._;()/:A-Z0-9]+").unwrap());
static UT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:WOS|MEDLINE|BCI|CCC|DIIDW|ZOOREC|PPRN|PQDT):[A-Z0-9._-]+").unwrap()
});
static FULL_RECORD_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/full-record/([^/?#]+)").unwrap());
static PAGE_RANGE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"([A-Za-z]?\d+)\s*[-\u{2013}\u{2014}]\s*([A-Za-z]?\d+)").unwrap());
static PAGE_SINGLE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b([A-Za-z]?\d+)\b").unwrap());
static SUMMARY_PATH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(.*?/summary/[^/]+/[^/]+/)(\d+)$").unwrap());
static PROSE_WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Za-z]{6,}\b").unwrap());
static PAGE_TAIL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Pages?\s*([A-Za-z0-9.\-]+)\s*$").unwrap());
static ISSUE_TAIL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Issue\s*([A-Za-z0-9.\-]+)\s*$").unwrap());

const DEFAULT_DEBUG_PORT: u16 = 9222;

pub fn normalize_doi_candidate(value: &str) -> String {
    match DOI_RE.find(value) {
        Some(m) => m
            .as_str()
            .trim_end_matches([' ', '.', ';', ',', ')', ']', '+'])
            .to_string(),
        None => String::new(),
    }
}

pub fn extract_ut_from_url(value: &str) -> String {
    if let Some(m) = UT_RE.find(value) {
        return m.as_str().to_ascii_uppercase();
    }
    FULL_RECORD_RE
        .captures(value)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

fn split_page_range(value: &str) -> (String, String) {
    let text = value.trim();
    if text.is_empty() {
        return (String::new(), String::new());
    }
    if let Some(caps) = PAGE_RANGE_RE.captures(text) {
        return (
            caps.get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            caps.get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
        );
    }
    if let Some(caps) = PAGE_SINGLE_RE.captures(text) {
        return (
            caps.get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            String::new(),
        );
    }
    (String::new(), String::new())
}

fn looks_like_prose(value: &str) -> bool {
    let text = value.trim();
    if text.is_empty() {
        return false;
    }
    text.chars().count() > 24 || PROSE_WORD_RE.is_match(text)
}

fn normalize_wos_metadata(volume: &str, issue: &str, pages: &str) -> (String, String, String) {
    let mut vol = volume.trim().to_string();
    let mut iss = issue.trim().to_string();
    let mut pgs = pages.trim().to_string();

    if let Some(m) = PAGE_TAIL_RE.captures(&vol) {
        let candidate_pages = m.get(1).map(|x| x.as_str().to_string()).unwrap_or_default();
        let start = m.get(0).unwrap().start();
        vol = vol[..start].trim_end().to_string();
        if pgs.is_empty() {
            pgs = candidate_pages;
        }
    }
    if let Some(m) = ISSUE_TAIL_RE.captures(&vol) {
        let candidate_issue = m.get(1).map(|x| x.as_str().to_string()).unwrap_or_default();
        let start = m.get(0).unwrap().start();
        vol = vol[..start].trim_end().to_string();
        if iss.is_empty() || looks_like_prose(&iss) {
            iss = candidate_issue;
        }
    }
    if looks_like_prose(&iss) {
        iss = String::new();
    }
    (vol, iss, pgs)
}

fn normalized_doi_candidates(values: &Value) -> Vec<String> {
    let items: Vec<Value> = match values {
        Value::Array(arr) => arr.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let text = match item {
            Value::String(s) => s,
            Value::Null => continue,
            other => other.to_string(),
        };
        let normalized = normalize_doi_candidate(&text);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(normalized);
    }
    out
}

fn value_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn first_nonblank(values: &[&str]) -> String {
    for v in values {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// JS payloads (preserved verbatim from Python)
// ---------------------------------------------------------------------------

const RESULTS_PAGE_READY_JS: &str = r#"
(() => {
  const text = String(document.body ? document.body.innerText : '');
  const fullRecordLinks = document.querySelectorAll('a[href*="/full-record/"], a[href*="full-record"]').length;
  const recordNodes = document.querySelectorAll('app-record, app-summary-record, [class*="summary-record" i], [class*="record-card" i]').length;
  const loadingText = /loading|please wait/i.test(text);
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i]'))
    .some(node => node.offsetParent !== null || node.getClientRects().length);
  const hasPositiveResults = /[1-9][\d,]*\s+results\s+from/i.test(text) || /\b[1-9][\d,]*\s+Documents\b/i.test(text);
  const noResults = !hasPositiveResults && (/\b0\s+results\s+from\b|\b0\s+Documents\b|no results found|no records found|your search did not match/i.test(text));
  return text.length > 500 && (fullRecordLinks > 0 || recordNodes > 0 || noResults) && !loadingText && !loadingNode;
})()
"#;

const FULL_RECORD_READY_JS: &str = r#"
(() => {
  const text = String(document.body ? document.body.innerText : '');
  const url = String(location.href || '');
  const title = String(document.title || '');
  const loadingText = /loading|please wait|正在加载|请稍候/i.test(text.slice(0, 1600));
  const loadingNode = Array.from(document.querySelectorAll('[aria-busy="true"], .mat-progress-spinner, mat-spinner, [class*="loading" i], [class*="spinner" i]'))
    .some(node => node.offsetParent !== null || node.getClientRects().length);
  const hasFullRecordUrl = url.includes('/full-record/');
  const hasDoi = /10\.\d{4,9}\/[\-._;()/:A-Z0-9]+/i.test(text) || document.querySelectorAll('[data-ta="FullRTa-DOI"], .doi-link, a[href*="doi.org/"]').length > 0;
  const hasTitle = !!document.querySelector('[data-ta*="FullRTa-title" i], [data-ta*="title" i] h2, h1, h2') || title.length > 8;
  const hasRecordSignal = /\b(Source|Abstract|Author Keywords|Keywords Plus|Document Type|Document Information|Accession Number|UT \(|Times Cited|Cited References|Publication Year|Volume|Issue|Pages)\b/i.test(text);
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
"#;

// Pulled verbatim from skills/wos_browser/scraper.py::collect_result_rows_via_dom.
// `__PAGE_INDEX__` is substituted with the integer page index at runtime.
const RESULT_ROWS_DOM_JS: &str = r#"
(async () => {
  const pageIndex = __PAGE_INDEX__;
  const doiRegex = /10\.\d{4,9}\/[\-._;()/:A-Z0-9]+/ig;
  const clean = (value) => (value || '').replace(/\s+/g, ' ').trim();
  const clip = (value, limit) => clean(value).slice(0, limit);
  const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));
  const bodyText = clean(document.body ? document.body.innerText : '');
  const totalResultsMatch = bodyText.match(/([\d,]+)\s+results\s+from/i);
  const totalPagesMatch = bodyText.match(/Page\s+\d+\s+of\s+(\d+)/i) || bodyText.match(/\bof\s+(\d+)\s+pages?\b/i);
  const totalResults = totalResultsMatch ? Number((totalResultsMatch[1] || '').replace(/,/g, '')) : 0;
  const totalPages = totalPagesMatch ? Number(totalPagesMatch[1]) : 0;
  const inferredPerPage = totalResults && totalPages ? Math.max(1, Math.round(totalResults / totalPages)) : 50;
  const pageFromUrlMatch = location.pathname.match(/\/(\d+)$/);
  const currentPageNumber = pageFromUrlMatch ? Number(pageFromUrlMatch[1]) : pageIndex;
  const expectedRankMin = Math.max(1, ((currentPageNumber - 1) * inferredPerPage) + 1);
  const expectedRankMax = currentPageNumber * inferredPerPage;
  const hasPositiveResults = /[1-9][\d,]*\s+results\s+from/i.test(bodyText) || /\b[1-9][\d,]*\s+Documents\b/i.test(bodyText);
  const noResults = !hasPositiveResults && (/\b0\s+results\s+from\b|\b0\s+Documents\b|no results found|no records found|your search did not match/i.test(bodyText));
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
      const normalized = item.replace(/[\s.,;)+]+$/g, '');
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
    const fullRecordMatch = text.match(/\/full-record\/([^/?#]+)/i);
    return fullRecordMatch ? fullRecordMatch[1] : '';
  };

  const linesFromText = (text) => String(text || '').split(/\n|\r| {2,}/).map(clean).filter(Boolean);

  const labeledValue = (lines, labels) => {
    for (let index = 0; index < lines.length; index++) {
      const line = lines[index];
      for (const label of labels) {
        const pattern = new RegExp(`^${label}\\s*[:\\-]?\\s*(.+)$`, 'i');
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
      const rankMatch = text.match(/^(\d+)\b/);
      const rank = rankMatch ? Number(rankMatch[1]) : 0;
      const titleNode = card.querySelector('a[href*="/full-record/"], a[href*="full-record"]');
      const title = clip(titleNode ? titleNode.innerText : '', 500);
      const fullRecordLink = titleNode ? titleNode.href : '';
      const utUniqueId = clip(extractUtFromUrl(fullRecordLink) || labeledValue(lines, ['UT \\(Unique WOS ID\\)', 'UT \\(Unique ID\\)', 'Accession Number', 'Unique ID', 'UT']), 120);
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
      const yearMatch = String(labeledYear || text.replace(doiRegex, ' ')).match(/(19|20)\d{2}/);
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
"#;

const FULL_RECORD_EXTRACT_JS: &str = r#"
(() => {
  const doiRegex = /10\.\d{4,9}\/[\-._;()/:A-Z0-9]+/ig;
  const clean = (value) => (value || '').replace(/\s+/g, ' ').trim();
  const clip = (value, limit) => clean(value).slice(0, limit);
  const pushMatches = (bucket, text) => {
    const found = String(text || '').match(doiRegex) || [];
    for (const item of found) {
      const normalized = item.replace(/[\s.,;)+]+$/g, '');
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
    const fullRecordMatch = text.match(/\/full-record\/([^/?#]+)/i);
    return fullRecordMatch ? fullRecordMatch[1] : '';
  };

  const linesFromText = (text) => String(text || '').split(/\n|\r| {2,}/).map(clean).filter(Boolean);

  const labeledValue = (lines, labels) => {
    for (let index = 0; index < lines.length; index++) {
      const line = lines[index];
      for (const label of labels) {
        const pattern = new RegExp(`^${label}\\s*[:\\-]?\\s*(.+)$`, 'i');
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
      const matched = labels.some((label) => new RegExp(`^${label}\\s*[:\\-]?\\s*(.*)$`, 'i').test(line));
      if (!matched) continue;
      const inline = labels
        .map((label) => line.match(new RegExp(`^${label}\\s*[:\\-]?\\s*(.+)$`, 'i')))
        .find(Boolean);
      if (inline && clean(inline[1])) return clean(inline[1]);
      const values = [];
      for (let cursor = index + 1; cursor < lines.length; cursor++) {
        const next = lines[cursor];
        if (stopLabels.some((label) => new RegExp(`^${label}\\s*[:\\-]?`, 'i').test(next))) break;
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
    (citedReferencesText.match(/\b\d{1,6}\b/) || [''])[0] ||
    (labeledValue(lines, ['Cited References', 'References']).match(/\b\d{1,6}\b/) || [''])[0],
    40
  );

  return {
    title: clip(firstText(['[data-ta*="FullRTa-title" i]', '[data-ta*="title" i] h2', 'h2', 'h1']) || clean(document.title), 500),
    sourceTitle: clip(firstText(['[data-ta*="SourceTitle" i]', '[data-ta*="source-title" i]']) || labeledValue(lines, ['Source Title', 'Source', 'Journal', 'Publication Title', 'Publication Name']), 500),
    authors: clip(firstText(['[data-ta*="author" i]', '[class*="author" i]']) || labeledValue(lines, ['Authors', 'Author(s)', 'By', 'Byline']), 1200),
    publicationYear: (labeledValue(lines, ['Publication Year', 'Published', 'Year', 'PY']).match(/(19|20)\d{2}/) || [''])[0],
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
    utUniqueId: clip(extractUtFromUrl(location.href) || labeledValue(lines, ['UT \\(Unique WOS ID\\)', 'UT \\(Unique ID\\)', 'Accession Number', 'Unique ID', 'UT']), 120),
    url: location.href,
    doiCandidates: Array.from(new Set(candidates)).slice(0, 10),
    textChars: document.body ? document.body.innerText.length : 0,
  };
})()
"#;

const CLICK_NEXT_PAGE_JS: &str = r#"
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
"#;

// ---------------------------------------------------------------------------
// Page operations
// ---------------------------------------------------------------------------

async fn wait_for_results_page_loaded(session: &mut CdpSession, timeout_seconds: f64) {
    wait_for_condition(session, RESULTS_PAGE_READY_JS, timeout_seconds, 0.25).await;
}

async fn collect_result_rows_via_dom(
    session: &mut CdpSession,
    page_index: u32,
) -> AppResult<Value> {
    let js = RESULT_ROWS_DOM_JS.replace("__PAGE_INDEX__", &page_index.to_string());
    evaluate_js_with(session, &js, true).await
}

async fn wait_for_full_record_page_ready(
    session: &mut CdpSession,
    timeout_seconds: f64,
) -> Map<String, Value> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds.max(3.0));
    let mut snapshot: Map<String, Value> = Map::new();
    while Instant::now() < deadline {
        let value = match evaluate_js(session, FULL_RECORD_READY_JS).await {
            Ok(v) => v,
            Err(_) => {
                sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        snapshot = value.as_object().cloned().unwrap_or_default();
        let has_full_record = snapshot
            .get("hasFullRecordUrl")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text_chars = snapshot
            .get("textChars")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let loading = snapshot
            .get("loading")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let enough_info = snapshot
            .get("enoughInfo")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if has_full_record && text_chars >= 900 && !loading && enough_info {
            return snapshot;
        }
        sleep(Duration::from_millis(250)).await;
    }
    snapshot
}

async fn extract_doi_from_full_record_page(
    full_record_url: &str,
    port: u16,
) -> AppResult<Map<String, Value>> {
    let target = open_debug_target(full_record_url, port).await?;
    if target.websocket_url.is_empty() {
        let _ = close_debug_target(&target.id, port).await;
        return Err(AppError::Browser(
            "The temporary full-record target does not expose a debugger websocket.".into(),
        ));
    }
    let target_id = target.id.clone();
    let result = (async {
        let mut session = CdpSession::connect(&target.websocket_url).await?;
        prepare_page_session(&mut session).await?;

        let current_url = evaluate_js(&mut session, "location.href")
            .await
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        if current_url.is_empty()
            || current_url == "about:blank"
            || !current_url.contains("/full-record/")
        {
            let nav = session
                .call("Page.navigate", json!({ "url": full_record_url }))
                .await;
            if nav.is_err() {
                let _ = goto_url(&mut session, full_record_url).await;
            }
        }

        let ready_snapshot = wait_for_full_record_page_ready(&mut session, 30.0).await;
        let has_full_record = ready_snapshot
            .get("hasFullRecordUrl")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text_chars = ready_snapshot
            .get("textChars")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let enough_info = ready_snapshot
            .get("enoughInfo")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !(has_full_record && text_chars >= 450 && enough_info) {
            session.close().await;
            return Err::<Map<String, Value>, AppError>(AppError::Browser(format!(
                "full-record page did not become ready: {ready_snapshot:?}"
            )));
        }

        let value = evaluate_js(&mut session, FULL_RECORD_EXTRACT_JS).await?;
        let map = value.as_object().cloned().unwrap_or_default();
        session.close().await;
        Ok(map)
    })
    .await;

    let _ = close_debug_target(&target_id, port).await;
    result
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

async fn click_next_page(session: &mut CdpSession) -> bool {
    matches!(
        evaluate_js(session, CLICK_NEXT_PAGE_JS).await,
        Ok(Value::Bool(true))
    )
}

pub fn build_next_page_url(current_url: &str) -> String {
    let Ok(parsed) = Url::parse(current_url) else {
        return String::new();
    };
    let path = parsed.path();
    let Some(caps) = SUMMARY_PATH_RE.captures(path) else {
        return String::new();
    };
    let prefix = caps.get(1).unwrap().as_str();
    let page_str = caps.get(2).unwrap().as_str();
    let Ok(page) = page_str.parse::<u32>() else {
        return String::new();
    };
    format!(
        "{}://{}{prefix}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        page + 1
    )
}

// ---------------------------------------------------------------------------
// Row preparation
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScrapedRecord {
    pub search_rank: u32,
    pub rank: u32,
    pub article_title: String,
    pub authors: String,
    pub source_title: String,
    pub doi: String,
    pub doi_url: String,
    pub publication_year: String,
    pub volume: String,
    pub issue: String,
    pub pages: String,
    pub start_page: String,
    pub end_page: String,
    pub document_type: String,
    pub abstract_text: String,
    pub author_keywords: String,
    pub keywords_plus: String,
    pub times_cited: String,
    pub cited_references_count: String,
    pub cited_references: String,
    pub ut_unique_id: String,
    pub source_page_url: String,
    pub source_page_index: u32,
    pub raw_snippet: String,
    pub full_record_link: String,
    pub doi_candidates: String,
    pub doi_recovery_source: String,
}

fn record_to_value(record: &ScrapedRecord) -> Value {
    let mut m = Map::new();
    m.insert("search_rank".into(), Value::from(record.search_rank));
    m.insert("rank".into(), Value::from(record.rank));
    m.insert(
        "Article Title".into(),
        Value::String(record.article_title.clone()),
    );
    m.insert("Authors".into(), Value::String(record.authors.clone()));
    m.insert(
        "Source Title".into(),
        Value::String(record.source_title.clone()),
    );
    m.insert("DOI".into(), Value::String(record.doi.clone()));
    m.insert(
        "Publication Year".into(),
        Value::String(record.publication_year.clone()),
    );
    m.insert("Volume".into(), Value::String(record.volume.clone()));
    m.insert("Issue".into(), Value::String(record.issue.clone()));
    m.insert("Pages".into(), Value::String(record.pages.clone()));
    m.insert(
        "Start Page".into(),
        Value::String(record.start_page.clone()),
    );
    m.insert("End Page".into(), Value::String(record.end_page.clone()));
    m.insert(
        "Document Type".into(),
        Value::String(record.document_type.clone()),
    );
    m.insert(
        "Abstract".into(),
        Value::String(record.abstract_text.clone()),
    );
    m.insert(
        "Author Keywords".into(),
        Value::String(record.author_keywords.clone()),
    );
    m.insert(
        "Keywords Plus".into(),
        Value::String(record.keywords_plus.clone()),
    );
    m.insert(
        "Times Cited".into(),
        Value::String(record.times_cited.clone()),
    );
    m.insert(
        "Cited References Count".into(),
        Value::String(record.cited_references_count.clone()),
    );
    m.insert(
        "Cited References".into(),
        Value::String(record.cited_references.clone()),
    );
    m.insert(
        "UT (Unique ID)".into(),
        Value::String(record.ut_unique_id.clone()),
    );
    m.insert(
        "source_page_url".into(),
        Value::String(record.source_page_url.clone()),
    );
    m.insert(
        "source_page_index".into(),
        Value::from(record.source_page_index),
    );
    m.insert(
        "raw_snippet".into(),
        Value::String(record.raw_snippet.clone()),
    );
    m.insert(
        "full_record_link".into(),
        Value::String(record.full_record_link.clone()),
    );
    m.insert(
        "doi_candidates".into(),
        Value::String(record.doi_candidates.clone()),
    );
    m.insert("doi_raw".into(), Value::String(record.doi.clone()));
    m.insert("doi_url".into(), Value::String(record.doi_url.clone()));
    if !record.doi_recovery_source.is_empty() {
        m.insert(
            "doi_recovery_source".into(),
            Value::String(record.doi_recovery_source.clone()),
        );
    }
    Value::Object(m)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct PageDiagnostics {
    pub page_index: u32,
    pub current_page_number: i64,
    pub expected_rank_min: i64,
    pub expected_rank_max: i64,
    pub records_seen: usize,
    pub doi_found: usize,
    pub page_wide_doi_found: usize,
    pub page_wide_doi_recovered: usize,
    pub detail_workers: usize,
    pub detail_pages_opened: usize,
    pub detail_errors: Vec<Value>,
    pub without_doi: usize,
    pub without_doi_ranks: Vec<u32>,
    pub no_results: bool,
    pub source_page_url: String,
    pub title: String,
    pub dom_error: String,
}

#[derive(Debug, Clone)]
struct PreparedRow {
    raw: Map<String, Value>,
    full_record_link: String,
}

fn map_field(m: &Map<String, Value>, key: &str) -> String {
    value_string(m.get(key))
}

fn build_final_record(
    row: &Map<String, Value>,
    fallback: &Map<String, Value>,
    doi_candidates: &[String],
    page_index: u32,
    page_url: &str,
    search_rank: u32,
    recovery_source: Option<&str>,
) -> Option<ScrapedRecord> {
    let normalized = doi_candidates
        .first()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if normalized.is_empty() {
        return None;
    }
    let article_title = first_nonblank(&[
        &map_field(fallback, "title"),
        &map_field(row, "articleTitle"),
        &map_field(row, "title"),
    ]);
    let source_title = first_nonblank(&[
        &map_field(fallback, "sourceTitle"),
        &map_field(row, "sourceTitle"),
    ]);
    let authors = first_nonblank(&[&map_field(fallback, "authors"), &map_field(row, "authors")]);
    let publication_year = first_nonblank(&[
        &map_field(fallback, "publicationYear"),
        &map_field(row, "publicationYear"),
        &map_field(row, "year"),
    ]);
    let volume_raw = first_nonblank(&[&map_field(fallback, "volume"), &map_field(row, "volume")]);
    let issue_raw = first_nonblank(&[&map_field(fallback, "issue"), &map_field(row, "issue")]);
    let pages_raw = first_nonblank(&[&map_field(fallback, "pages"), &map_field(row, "pages")]);
    let (volume, issue, pages) = normalize_wos_metadata(&volume_raw, &issue_raw, &pages_raw);
    let (start_page, end_page) = split_page_range(&pages);
    let document_type = first_nonblank(&[
        &map_field(fallback, "documentType"),
        &map_field(row, "documentType"),
    ]);
    let author_keywords = first_nonblank(&[
        &map_field(fallback, "authorKeywords"),
        &map_field(row, "authorKeywords"),
    ]);
    let keywords_plus = first_nonblank(&[
        &map_field(fallback, "keywordsPlus"),
        &map_field(row, "keywordsPlus"),
    ]);
    let abstract_text = first_nonblank(&[
        &map_field(fallback, "abstractText"),
        &map_field(row, "abstractText"),
    ]);
    let times_cited = first_nonblank(&[
        &map_field(fallback, "timesCited"),
        &map_field(row, "timesCited"),
    ]);
    let cited_references_count = first_nonblank(&[
        &map_field(fallback, "citedReferencesCount"),
        &map_field(row, "citedReferencesCount"),
    ]);
    let cited_references_text = first_nonblank(&[
        &map_field(fallback, "citedReferencesText"),
        &map_field(row, "citedReferencesText"),
    ]);
    let full_record_link = map_field(row, "fullRecordLink");
    let ut_unique_id = first_nonblank(&[
        &map_field(fallback, "utUniqueId"),
        &map_field(row, "utUniqueId"),
        &extract_ut_from_url(&full_record_link),
    ]);
    let rank = row.get("rank").and_then(Value::as_u64).unwrap_or(0) as u32;
    let raw_snippet = map_field(row, "raw_snippet");
    Some(ScrapedRecord {
        search_rank,
        rank,
        article_title,
        authors,
        source_title,
        doi: normalized.clone(),
        doi_url: format!("https://doi.org/{normalized}"),
        publication_year,
        volume,
        issue,
        pages,
        start_page,
        end_page,
        document_type,
        abstract_text,
        author_keywords,
        keywords_plus,
        times_cited,
        cited_references_count,
        cited_references: cited_references_text,
        ut_unique_id,
        source_page_url: page_url.to_string(),
        source_page_index: page_index,
        raw_snippet,
        full_record_link,
        doi_candidates: doi_candidates.join("; "),
        doi_recovery_source: recovery_source.unwrap_or("").to_string(),
    })
}

async fn extract_records_via_dom(
    session: &mut CdpSession,
    page_index: u32,
    port: u16,
    detail_workers: usize,
    max_records: usize,
    page_url: &str,
) -> AppResult<(Vec<ScrapedRecord>, PageDiagnostics)> {
    let payload = match collect_result_rows_via_dom(session, page_index).await {
        Ok(v) => v,
        Err(err) => {
            let diag = PageDiagnostics {
                page_index,
                current_page_number: page_index as i64,
                dom_error: err.to_string().chars().take(500).collect(),
                ..PageDiagnostics::default()
            };
            return Ok((Vec::new(), diag));
        }
    };
    let payload_obj = payload.as_object().cloned().unwrap_or_default();
    let rows: Vec<Map<String, Value>> = payload_obj
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_object().cloned())
        .collect();
    let page_wide_doi: Vec<Value> = payload_obj
        .get("pageWideDoiCandidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let no_results = payload_obj
        .get("noResults")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut prepared_rows: Vec<PreparedRow> = Vec::new();
    let limit = max_records;
    for row in &rows {
        if limit > 0 && prepared_rows.len() >= limit {
            break;
        }
        let full_record_link = map_field(row, "fullRecordLink");
        let row_dois = normalized_doi_candidates(row.get("doiCandidates").unwrap_or(&Value::Null));
        if full_record_link.is_empty() && row_dois.is_empty() {
            continue;
        }
        prepared_rows.push(PreparedRow {
            raw: row.clone(),
            full_record_link,
        });
    }

    let worker_count = detail_workers.clamp(1, 8);

    // Fetch full-record details concurrently for rows that have a link.
    let detail_rows: Vec<(usize, String)> = prepared_rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            if r.full_record_link.is_empty() {
                None
            } else {
                Some((i, r.full_record_link.clone()))
            }
        })
        .collect();

    let mut detail_by_index: HashMap<usize, (Map<String, Value>, String)> = HashMap::new();
    if !detail_rows.is_empty() {
        let semaphore = Arc::new(Semaphore::new(worker_count));
        let mut futs: FuturesUnordered<_> = FuturesUnordered::new();
        for (index, link) in &detail_rows {
            let sem = semaphore.clone();
            let link = link.clone();
            let idx = *index;
            futs.push(async move {
                let _permit = sem.acquire_owned().await.ok();
                let result = extract_doi_from_full_record_page(&link, port).await;
                (idx, result)
            });
        }
        while let Some((index, result)) = futs.next().await {
            match result {
                Ok(map) => {
                    detail_by_index.insert(index, (map, String::new()));
                }
                Err(err) => {
                    detail_by_index.insert(index, (Map::new(), err.to_string()));
                }
            }
        }
    }

    let mut final_rows: Vec<ScrapedRecord> = Vec::new();
    let mut seen_doi: HashSet<String> = HashSet::new();
    let mut detail_errors: Vec<Value> = Vec::new();
    let mut missing_rows: Vec<(usize, Map<String, Value>)> = Vec::new();
    let mut rows_without_doi: Vec<u32> = Vec::new();

    for (index, prepared) in prepared_rows.iter().enumerate() {
        let (fallback, error) = detail_by_index
            .get(&index)
            .cloned()
            .unwrap_or_else(|| (Map::new(), String::new()));
        if !error.is_empty() {
            let mut entry = Map::new();
            entry.insert(
                "rank".into(),
                Value::from(
                    prepared
                        .raw
                        .get("rank")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                ),
            );
            entry.insert(
                "full_record_link".into(),
                Value::String(prepared.full_record_link.clone()),
            );
            entry.insert("error".into(), Value::String(error));
            detail_errors.push(Value::Object(entry));
        }
        let row_dois =
            normalized_doi_candidates(prepared.raw.get("doiCandidates").unwrap_or(&Value::Null));
        let fallback_dois =
            normalized_doi_candidates(fallback.get("doiCandidates").unwrap_or(&Value::Null));
        let mut combined: Vec<String> = Vec::new();
        for d in row_dois.into_iter().chain(fallback_dois) {
            if !combined.contains(&d) {
                combined.push(d);
            }
        }

        if !combined.is_empty() {
            let normalized = combined[0].clone();
            if !seen_doi.contains(&normalized) {
                if let Some(record) = build_final_record(
                    &prepared.raw,
                    &fallback,
                    &combined,
                    page_index,
                    page_url,
                    (final_rows.len() + 1) as u32,
                    None,
                ) {
                    seen_doi.insert(normalized);
                    final_rows.push(record);
                }
            }
            continue;
        }

        rows_without_doi.push(
            prepared
                .raw
                .get("rank")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        );
        missing_rows.push((index, fallback));
    }

    // Page-wide single-unused DOI rescue (matches Python).
    let mut page_wide_unused: Vec<String> =
        normalized_doi_candidates(&Value::Array(page_wide_doi.clone()))
            .into_iter()
            .filter(|d| !seen_doi.contains(d))
            .collect();
    let mut recovered_from_page_scan: usize = 0;
    if missing_rows.len() == 1 && page_wide_unused.len() == 1 {
        let (idx, fallback) = missing_rows.remove(0);
        let recovered_doi = page_wide_unused.remove(0);
        let prepared = &prepared_rows[idx];
        if let Some(record) = build_final_record(
            &prepared.raw,
            &fallback,
            std::slice::from_ref(&recovered_doi),
            page_index,
            page_url,
            (final_rows.len() + 1) as u32,
            Some("page_wide_single_unused"),
        ) {
            seen_doi.insert(recovered_doi);
            final_rows.push(record);
            recovered_from_page_scan = 1;
            rows_without_doi.clear();
        }
    }

    let mut diag = PageDiagnostics {
        page_index,
        ..PageDiagnostics::default()
    };
    diag.current_page_number = payload_obj
        .get("currentPageNumber")
        .and_then(Value::as_i64)
        .unwrap_or(page_index as i64);
    diag.expected_rank_min = payload_obj
        .get("expectedRankMin")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    diag.expected_rank_max = payload_obj
        .get("expectedRankMax")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    diag.records_seen = prepared_rows.len();
    diag.doi_found = final_rows.len();
    diag.page_wide_doi_found = page_wide_doi
        .iter()
        .map(|v| normalize_doi_candidate(value_string(Some(v)).as_str()))
        .filter(|d| !d.is_empty())
        .collect::<HashSet<_>>()
        .len();
    diag.page_wide_doi_recovered = recovered_from_page_scan;
    diag.detail_workers = worker_count;
    diag.detail_pages_opened = detail_rows.len();
    diag.detail_errors = detail_errors.into_iter().take(20).collect();
    diag.without_doi = rows_without_doi.len();
    diag.without_doi_ranks = rows_without_doi.into_iter().filter(|r| *r > 0).collect();
    diag.no_results = no_results;
    diag.source_page_url = page_url.to_string();

    Ok((final_rows, diag))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ScrapeResult {
    pub records: Vec<Value>,
    pub pages_scraped: usize,
    pub page_diagnostics: Vec<PageDiagnostics>,
    pub records_seen_total: usize,
    pub without_doi_total: usize,
    pub target_title: String,
    pub target_url: String,
}

pub async fn scrape_wos_pages(
    port: u16,
    max_pages: u32,
    wait_seconds: f64,
    detail_workers: usize,
    max_records: usize,
) -> AppResult<ScrapeResult> {
    let target = find_wos_results_target(port).await?;
    if target.websocket_url.is_empty() {
        return Err(AppError::Browser(
            "The active browser target does not expose a debugger websocket.".into(),
        ));
    }
    let mut session = CdpSession::connect(&target.websocket_url).await?;
    prepare_page_session(&mut session).await?;

    let mut all_records: Vec<Value> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut page_diagnostics: Vec<PageDiagnostics> = Vec::new();

    for page_index in 1..=max_pages {
        let remaining = if max_records > 0 {
            max_records.saturating_sub(all_records.len())
        } else {
            0
        };
        if max_records > 0 && remaining == 0 {
            break;
        }

        wait_for_results_page_loaded(&mut session, wait_seconds.max(10.0)).await;
        let snapshot = get_page_snapshot(&mut session, 120_000).await;
        let page_url = snapshot.url.clone();
        if !page_url.to_ascii_lowercase().contains("webofscience") {
            session.close().await;
            return Err(AppError::Browser(
                "The active page does not look like a Web of Science page.".into(),
            ));
        }

        let (page_records, mut diag) = extract_records_via_dom(
            &mut session,
            page_index,
            port,
            detail_workers,
            remaining,
            &page_url,
        )
        .await?;
        diag.title = snapshot.title.clone();

        for record in &page_records {
            let doi = normalize_doi_candidate(&record.doi).to_ascii_lowercase();
            let key = if !doi.is_empty() {
                format!("doi:{doi}")
            } else if !record.ut_unique_id.is_empty() {
                format!("ut:{}", record.ut_unique_id.to_ascii_uppercase())
            } else if !record.full_record_link.is_empty() {
                format!("url:{}", record.full_record_link.trim_end_matches('/'))
            } else {
                String::new()
            };
            if key.is_empty() || !seen_keys.insert(key) {
                continue;
            }
            all_records.push(record_to_value(record));
            if max_records > 0 && all_records.len() >= max_records {
                break;
            }
        }

        page_diagnostics.push(diag.clone());
        if page_index >= max_pages {
            break;
        }
        if max_records > 0 && all_records.len() >= max_records {
            break;
        }
        if diag.no_results {
            break;
        }

        let next_url = build_next_page_url(&page_url);
        if !next_url.is_empty() {
            if !goto_url(&mut session, &next_url).await.unwrap_or(false) {
                break;
            }
        } else if !click_next_page(&mut session).await {
            break;
        }

        // Wait for the URL to actually change.
        let nav_deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < nav_deadline {
            let new_url = evaluate_js(&mut session, "location.href")
                .await
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            if !new_url.is_empty() && new_url != page_url {
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }
        wait_for_results_page_loaded(&mut session, wait_seconds.max(10.0)).await;
    }

    let result = ScrapeResult {
        pages_scraped: page_diagnostics.len(),
        records_seen_total: page_diagnostics.iter().map(|d| d.records_seen).sum(),
        without_doi_total: page_diagnostics.iter().map(|d| d.without_doi).sum(),
        target_title: target.title.clone(),
        target_url: target.url.clone(),
        records: all_records,
        page_diagnostics,
    };
    session.close().await;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_doi_strips_trailing_punctuation() {
        assert_eq!(
            normalize_doi_candidate("10.1234/abc.efg)"),
            "10.1234/abc.efg"
        );
        assert_eq!(
            normalize_doi_candidate("foo 10.5555/x;bar."),
            "10.5555/x;bar"
        );
        assert_eq!(normalize_doi_candidate("no doi here"), "");
    }

    #[test]
    fn extract_ut_handles_prefixed_and_url_forms() {
        assert_eq!(extract_ut_from_url("WOS:000123456789"), "WOS:000123456789");
        assert_eq!(
            extract_ut_from_url("https://x.com/wos/woscc/full-record/WOS:000123"),
            "WOS:000123"
        );
        assert_eq!(
            extract_ut_from_url("https://x.com/wos/woscc/full-record/abc"),
            "abc"
        );
    }

    #[test]
    fn next_page_url_increments() {
        let url = "https://www.webofscience.com/wos/woscc/summary/abc/relevance/1";
        let next = build_next_page_url(url);
        assert!(next.ends_with("/2"));
    }

    #[test]
    fn metadata_cleanup_peels_pages_tail() {
        let (vol, iss, pgs) = normalize_wos_metadata("Volume 56 Pages 2002-2006", "Issue 12", "");
        assert!(vol.starts_with("Volume 56"));
        assert!(!vol.contains("Pages"));
        assert_eq!(iss, "Issue 12");
        assert_eq!(pgs, "2002-2006");
    }
}
