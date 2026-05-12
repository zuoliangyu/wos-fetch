from __future__ import annotations

import random
import re
import threading
import time
from typing import Any
from urllib.parse import urljoin, urlsplit, urlunsplit

import requests
from bs4 import BeautifulSoup

try:
    import html2text
except ImportError:
    html2text = None


USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/124.0.0.0 Safari/537.36"
)
DEFAULT_TIMEOUT = 60
HTML_MIN_CHARS = 6000

# Politeness floor between same-host requests. Even at workers=10, no single
# publisher will see more than ~1 GET per second from us. Several Chinese-
# university IP segments have been banned for batch scraping Elsevier; this
# throttle is the last line of defence against the user dialing concurrency up.
MIN_INTER_REQUEST_SECONDS_PER_HOST = 1.0
_HOST_LAST_REQUEST: dict[str, float] = {}
_HOST_LOCK = threading.Lock()


def _throttle_per_host(url: str, min_seconds: float = MIN_INTER_REQUEST_SECONDS_PER_HOST) -> None:
    if min_seconds <= 0:
        return
    try:
        host = urlsplit(str(url or "")).netloc.lower()
    except Exception:
        return
    if not host:
        return
    with _HOST_LOCK:
        last = _HOST_LAST_REQUEST.get(host, 0.0)
        now = time.monotonic()
        scheduled = max(now, last + min_seconds)
        _HOST_LAST_REQUEST[host] = scheduled
        wait = scheduled - now
    if wait > 0:
        time.sleep(wait + random.uniform(0, 0.3))

_PUBLISHER_MAP: dict[str, tuple[str, str]] = {
    "10.1016": ("Elsevier (ScienceDirect)", "https://www.sciencedirect.com"),
    "10.1038": ("Nature Portfolio", "https://www.nature.com"),
    "10.1039": ("Royal Society of Chemistry", "https://pubs.rsc.org"),
    "10.1021": ("ACS Publications", "https://pubs.acs.org"),
    "10.1002": ("Wiley Online Library", "https://onlinelibrary.wiley.com"),
    "10.1111": ("Wiley Online Library", "https://onlinelibrary.wiley.com"),
    "10.1007": ("Springer", "https://link.springer.com"),
    "10.1186": ("BioMed Central", "https://www.biomedcentral.com"),
    "10.1080": ("Taylor & Francis", "https://www.tandfonline.com"),
    "10.1093": ("Oxford University Press", "https://academic.oup.com"),
    "10.1126": ("Science (AAAS)", "https://www.science.org"),
    "10.1136": ("BMJ", "https://www.bmj.com"),
    "10.1073": ("PNAS", "https://www.pnas.org"),
    "10.1103": ("American Physical Society", "https://journals.aps.org"),
    "10.1017": ("Cambridge University Press", "https://www.cambridge.org"),
    "10.1109": ("IEEE Xplore", "https://ieeexplore.ieee.org"),
    "10.1145": ("ACM Digital Library", "https://dl.acm.org"),
    "10.1056": ("New England Journal of Medicine", "https://www.nejm.org"),
    "10.1001": ("JAMA Network", "https://jamanetwork.com"),
    "10.1177": ("SAGE Journals", "https://journals.sagepub.com"),
    "10.1097": ("Wolters Kluwer", "https://journals.lww.com"),
    "10.1371": ("PLOS", "https://plos.org"),
    "10.3390": ("MDPI", "https://www.mdpi.com"),
    "10.3389": ("Frontiers", "https://www.frontiersin.org"),
    "10.1152": ("American Physiological Society", "https://journals.physiology.org"),
    "10.1128": ("American Society for Microbiology", "https://journals.asm.org"),
    "10.4049": ("The Journal of Immunology", "https://www.jimmunol.org"),
    "10.2147": ("Dove Medical Press", "https://www.dovepress.com"),
}

_DOI_PREFIX_RE = re.compile(r"^10\.\d{4,9}")


def publisher_from_doi(doi: str) -> tuple[str, str] | None:
    normalized = normalize_doi(doi)
    match = _DOI_PREFIX_RE.match(normalized)
    if not match:
        return None
    return _PUBLISHER_MAP.get(match.group())


def get_required_publishers(dois: list[str]) -> list[dict[str, Any]]:
    seen: dict[str, dict[str, Any]] = {}
    for doi in dois:
        info = publisher_from_doi(doi)
        if info is None:
            continue
        name, url = info
        if name not in seen:
            seen[name] = {"name": name, "url": url, "count": 0}
        seen[name]["count"] += 1
    return list(seen.values())


def normalize_doi(value: Any) -> str:
    if value is None:
        return ""
    doi = str(value).strip()
    doi = re.sub(r"^https?://(dx\.)?doi\.org/", "", doi, flags=re.IGNORECASE)
    return doi.strip()


def get_session() -> requests.Session:
    session = requests.Session()
    session.headers.update({"User-Agent": USER_AGENT, "Accept": "text/html,application/xhtml+xml,*/*"})
    return session


def resolve_doi(doi: str, session: requests.Session | None = None, timeout: int = DEFAULT_TIMEOUT) -> requests.Response:
    normalized = normalize_doi(doi)
    if not normalized:
        raise ValueError("missing_doi")
    active_session = session or get_session()
    _throttle_per_host(f"https://doi.org/{normalized}")
    response = active_session.get(f"https://doi.org/{normalized}", allow_redirects=True, timeout=timeout)
    response.raise_for_status()
    return response


def resolve_doi_landing_url(doi: str, timeout: int = DEFAULT_TIMEOUT) -> str:
    return resolve_doi(doi, timeout=timeout).url


def _normalize_url(url: str, base_url: str) -> str:
    normalized = urljoin(base_url, str(url or "").strip())
    if not normalized:
        return ""
    parts = urlsplit(normalized)
    return urlunsplit((parts.scheme, parts.netloc, parts.path, parts.query, ""))


def _replace_url_path(url: str, source: str, target: str) -> str:
    parts = urlsplit(url)
    if source not in parts.path:
        return ""
    return urlunsplit((parts.scheme, parts.netloc, parts.path.replace(source, target, 1), parts.query, parts.fragment))


def build_candidate_urls(landing_url: str, html: str = "") -> list[str]:
    candidates: list[str] = []
    lowered = str(landing_url or "").lower()
    pii_match = re.search(r"/(?:retrieve/pii|science/article/(?:abs/)?pii)/([A-Z0-9]+)", landing_url, flags=re.IGNORECASE)
    pii = pii_match.group(1) if pii_match else ""

    if "sciencedirect.com" in lowered:
        candidates.append(_replace_url_path(landing_url, "/science/article/abs/pii/", "/science/article/pii/"))
        candidates.append(_replace_url_path(landing_url, "/science/article/abs/", "/science/article/"))
    if ("sciencedirect.com" in lowered or "linkinghub.elsevier.com" in lowered) and pii:
        candidates.append(f"https://www.sciencedirect.com/science/article/pii/{pii}")
    if any(host in lowered for host in ["wiley.com", "onlinelibrary.wiley.com", "tandfonline.com", "sagepub.com"]):
        candidates.append(_replace_url_path(landing_url, "/doi/abs/", "/doi/full/"))

    if html:
        soup = BeautifulSoup(html, "html.parser")
        for name in ("citation_fulltext_html_url", "citation_full_html_url", "citation_html_url"):
            meta = soup.find("meta", attrs={"name": name})
            if meta and meta.get("content"):
                candidates.append(_normalize_url(str(meta["content"]), landing_url))
        for link in soup.find_all("a", href=True):
            href = str(link.get("href", "") or "").strip()
            label = link.get_text(" ", strip=True).lower()
            href_lower = href.lower()
            if any(token in label for token in ["full text", "full article", "html"]) or any(
                token in href_lower for token in ["/fulltext", "/doi/full"]
            ):
                candidates.append(_normalize_url(href, landing_url))

    seen: set[str] = set()
    deduped: list[str] = []
    landing_normalized = _normalize_url(landing_url, landing_url)
    for candidate in candidates:
        normalized = _normalize_url(candidate, landing_url)
        if not normalized or normalized == landing_normalized or normalized in seen:
            continue
        seen.add(normalized)
        deduped.append(normalized)
    return deduped


def _drop_noise_nodes(root: BeautifulSoup) -> None:
    for tag in root.select(
        ",".join([
            "script", "style", "noscript", "svg", "canvas", "nav", "header", "footer", "aside",
            "form", "dialog", "button", "input", "select", "textarea", ".cookie", ".cookies",
            ".advertisement", ".ads", ".share", ".social", ".related", ".recommended",
        ])
    ):
        tag.decompose()


def _normalize_text(text: str) -> str:
    cleaned = str(text or "").replace("\r", "\n")
    cleaned = re.sub(r"[ \t\f\v]+", " ", cleaned)
    cleaned = re.sub(r"\n{3,}", "\n\n", cleaned)
    return cleaned.strip()


def _html_to_text(fragment_html: str) -> str:
    if not fragment_html:
        return ""
    soup = BeautifulSoup(fragment_html, "html.parser")
    _drop_noise_nodes(soup)
    if html2text is not None:
        converter = html2text.HTML2Text()
        converter.body_width = 0
        converter.ignore_links = True
        converter.ignore_images = True
        converter.ignore_tables = False
        text = converter.handle(str(soup))
    else:
        text = soup.get_text("\n", strip=True)
    return _normalize_text(text)


def _score_text(text: str) -> tuple[int, int, int]:
    lowered = str(text or "").lower()
    section_hits = sum(marker in lowered for marker in ["abstract", "introduction", "methods", "results", "discussion", "references"])
    noise_hits = sum(marker in lowered for marker in ["cookie", "sign in", "recommended articles", "related articles"])
    paragraphs = max(lowered.count("\n\n"), lowered.count("\n"))
    return (len(text), section_hits - noise_hits, paragraphs)


def extract_html_text(html: str) -> str:
    soup = BeautifulSoup(html, "html.parser")
    _drop_noise_nodes(soup)
    candidates: list[str] = []
    selectors = [
        "article", "main", '[role="main"]', ".article", ".article-body", ".article-content",
        ".article__body", ".article-text", ".main-content", "#main-content", "#article-content",
        "[data-article-body]", "[data-test='article-body']",
    ]
    for selector in selectors:
        for node in soup.select(selector):
            text = _html_to_text(str(node))
            if text:
                candidates.append(text)
    fallback = _html_to_text(str(soup.body or soup))
    if fallback:
        candidates.append(fallback)
    return _normalize_text(max(candidates, key=_score_text, default=""))


def acquire_article_fulltext(
    doi: str,
    *,
    prefer: str = "web-first",
    timeout: int = DEFAULT_TIMEOUT,
    html_min_chars: int = HTML_MIN_CHARS,
) -> dict[str, Any]:
    del prefer
    normalized = normalize_doi(doi)
    result: dict[str, Any] = {
        "doi_normalized": normalized,
        "landing_url": "",
        "source_url": "",
        "acquisition_type": "",
        "acquisition_status": "",
        "content_text": "",
        "content_chars": 0,
        "error_message": "",
    }
    if not normalized:
        result.update({"acquisition_status": "error", "error_message": "missing_doi"})
        return result

    with get_session() as session:
        try:
            response = resolve_doi(normalized, session=session, timeout=timeout)
            landing_url = response.url
            result["landing_url"] = landing_url

            content_type = response.headers.get("Content-Type", "").lower()
            if content_type and "html" not in content_type and "xml" not in content_type:
                result.update({
                    "source_url": landing_url,
                    "acquisition_type": "non_html",
                    "acquisition_status": "blocked",
                    "error_message": f"non_html_response:{content_type[:80]}",
                })
                return result

            html = response.text
            best_source_url = landing_url
            best_text = extract_html_text(html)
            if len(best_text) >= html_min_chars:
                result.update({"source_url": landing_url, "acquisition_type": "html", "acquisition_status": "ok", "content_text": best_text, "content_chars": len(best_text)})
                return result

            candidate_urls = build_candidate_urls(landing_url, html)
            for candidate_url in candidate_urls:
                try:
                    _throttle_per_host(candidate_url)
                    candidate_response = session.get(candidate_url, allow_redirects=True, timeout=timeout)
                    candidate_response.raise_for_status()
                except Exception:
                    continue
                candidate_ct = candidate_response.headers.get("Content-Type", "").lower()
                if candidate_ct and "html" not in candidate_ct and "xml" not in candidate_ct:
                    continue
                candidate_text = extract_html_text(candidate_response.text)
                if _score_text(candidate_text) > _score_text(best_text):
                    best_text = candidate_text
                    best_source_url = candidate_response.url
                if len(candidate_text) >= html_min_chars:
                    result.update({"source_url": candidate_response.url, "acquisition_type": "html", "acquisition_status": "ok", "content_text": candidate_text, "content_chars": len(candidate_text)})
                    return result

            status = "insufficient_html" if best_text else "blocked"
            error_message = "html_too_short" if best_text else "no_usable_content"
            result.update({
                "source_url": best_source_url,
                "acquisition_type": "html",
                "acquisition_status": status,
                "content_text": best_text,
                "content_chars": len(best_text),
                "error_message": error_message,
            })
            return result
        except Exception as exc:
            result.update({"acquisition_status": "error", "error_message": str(exc)[:300]})
            return result


def acquire_article_fulltext_with_browser_fallback(
    doi: str,
    *,
    debug_port: int = 9222,
    timeout: int = DEFAULT_TIMEOUT,
    html_min_chars: int = HTML_MIN_CHARS,
    browser_wait_seconds: float = 25.0,
) -> dict[str, Any]:
    result = acquire_article_fulltext(doi, timeout=timeout, html_min_chars=html_min_chars)
    if result.get("acquisition_status") == "ok":
        return result

    landing_url = result.get("landing_url") or ""
    if not landing_url:
        normalized = normalize_doi(doi)
        if normalized:
            landing_url = f"https://doi.org/{normalized}"
    if not landing_url:
        return result

    try:
        from skills.wos_browser.browser import fetch_fulltext_via_browser
        browser_payload = fetch_fulltext_via_browser(
            url=landing_url,
            port=debug_port,
            wait_seconds=browser_wait_seconds,
            min_text_chars=html_min_chars,
        )
        text = str(browser_payload.get("articleText") or browser_payload.get("bodyText") or "")
        source_url = str(browser_payload.get("url") or landing_url)
        if browser_payload.get("challengeDetected"):
            result.update({"acquisition_status": "blocked", "acquisition_type": "browser_html",
                           "source_url": source_url, "error_message": "browser_challenge_detected"})
        elif text and len(text) >= html_min_chars:
            result.update({"acquisition_status": "ok", "acquisition_type": "browser_html",
                           "content_text": text, "content_chars": len(text), "source_url": source_url, "error_message": ""})
        elif text:
            result.update({"acquisition_status": "insufficient_html", "acquisition_type": "browser_html",
                           "content_text": text, "content_chars": len(text), "source_url": source_url})
        else:
            result["acquisition_type"] = "browser_html"
    except Exception as exc:
        result["browser_fallback_error"] = str(exc)[:300]
    return result
