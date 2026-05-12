from __future__ import annotations

import io
import re
import zipfile
from typing import Any

import pandas as pd
from schemas.extraction_template import REVIEW_EVIDENCE_EXPORT_COLUMNS

EXCEL_CELL_CHAR_LIMIT = 32767
FULLTEXT_DIR = "fulltext"
EXCEL_ILLEGAL_CHARACTERS_RE = re.compile(r"[\x00-\x08\x0B-\x0C\x0E-\x1F]")

SECTION_ALIASES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("title", ("title", "article title")),
    ("abstract", ("abstract", "summary")),
    ("introduction", ("introduction", "background", "overview")),
    ("methods", ("methods", "materials and methods", "methodology", "experimental", "experiment", "experiments")),
    ("results", ("results", "findings")),
    ("discussion", ("discussion", "results and discussion")),
    ("conclusion", ("conclusion", "conclusions", "concluding remarks")),
    ("references", ("references", "bibliography", "literature cited", "works cited")),
)

UT_FILENAME_COLUMNS = (
    "UT (Unique ID)", "UT (Unique WOS ID)", "ut_unique_id", "ut_unique_wos_id",
    "UT", "ut", "Accession Number", "accession_number",
)


_CSV_ENCODING_CANDIDATES = ("utf-8-sig", "utf-8", "gb18030", "cp936", "latin-1")


def _read_csv_with_encoding_fallback(file_bytes: bytes) -> pd.DataFrame:
    """Excel on Chinese Windows often saves CSV as GB18030 or UTF-8 with BOM,
    not plain UTF-8. Try a sensible sequence before giving up."""
    last_error: Exception | None = None
    for encoding in _CSV_ENCODING_CANDIDATES:
        try:
            return pd.read_csv(io.BytesIO(file_bytes), encoding=encoding)
        except (UnicodeDecodeError, UnicodeError) as exc:
            last_error = exc
            continue
    try:
        return pd.read_csv(io.BytesIO(file_bytes), encoding="utf-8", encoding_errors="replace")
    except Exception as exc:
        raise ValueError(f"Could not decode CSV; tried {_CSV_ENCODING_CANDIDATES}. Last error: {last_error or exc}")


def read_table(file_bytes: bytes, filename: str) -> pd.DataFrame:
    name = str(filename or "").lower()
    if name.endswith(".csv"):
        return _read_csv_with_encoding_fallback(file_bytes)
    if name.endswith((".xlsx", ".xls")):
        return pd.read_excel(io.BytesIO(file_bytes))
    if name.endswith(".zip"):
        return read_zip_table(file_bytes)
    raise ValueError("Only .xlsx, .xls, .csv, and .zip files are supported.")


def read_zip_table(file_bytes: bytes) -> pd.DataFrame:
    with zipfile.ZipFile(io.BytesIO(file_bytes)) as archive:
        names = archive.namelist()
        preferred = [name for name in names if name.lower().endswith((".xlsx", ".xls", ".csv")) and not name.endswith("/")]
        if not preferred:
            raise ValueError("ZIP does not contain a supported table file.")
        name = sorted(preferred, key=lambda item: (not item.lower().endswith(".xlsx"), item))[0]
        table = read_table(archive.read(name), name)
        if "content_text" in table.columns:
            for index, value in table["content_text"].items():
                path = str(value or "").replace("\\", "/")
                if not path.startswith(f"{FULLTEXT_DIR}/") or path not in names:
                    continue
                try:
                    table.at[index, "content_text"] = archive.read(path).decode("utf-8", errors="ignore")
                except Exception:
                    continue
        return table


def first_existing_column(df: pd.DataFrame, candidates: list[str]) -> str | None:
    for column in candidates:
        if column in df.columns:
            return column
    return None


def flatten_json_column(df: pd.DataFrame, column_name: str) -> pd.DataFrame:
    if column_name not in df.columns:
        return df
    objects = [value if isinstance(value, dict) else {} for value in df[column_name]]
    if not any(objects):
        return df
    flat = pd.json_normalize(objects)
    flat.index = df.index
    flat.columns = [str(column) for column in flat.columns]
    for column in flat.columns:
        if column not in df.columns:
            df[column] = flat[column]
        else:
            existing = df[column]
            blank_mask = existing.isna() | existing.astype(str).str.strip().eq("")
            df[column] = existing.where(~blank_mask, flat[column])
    return df


def prepare_export_table(table: pd.DataFrame) -> pd.DataFrame:
    output = table.copy()
    export_aliases = {
        "Article Title": ["Article Title", "article_title", "title", "Document Title", "中文标题"],
        "Authors": ["Authors", "authors", "Author Full Names", "Author(s)", "Byline", "basic_info_authors"],
        "Source Title": ["Source Title", "source_title", "journal", "journal_title", "Publication Name", "Publication Title", "basic_info_journal"],
        "DOI": ["doi_normalized", "DOI", "doi", "basic_info_doi"],
        "Publication Year": ["Publication Year", "publication_year", "year", "published_year", "Pub Year", "PY", "basic_info_year"],
        "Volume": ["Volume", "volume", "Vol", "basic_info_volume"],
        "Issue": ["Issue", "issue", "Number", "No", "basic_info_issue"],
        "Pages": ["Pages", "pages", "Page Range", "page_range", "basic_info_pages"],
        "Start Page": ["Start Page", "start_page", "Beginning Page", "BP"],
        "End Page": ["End Page", "end_page", "Ending Page", "EP"],
        "Document Type": ["Document Type", "document_type", "record_type", "Publication Type", "basic_info_document_type"],
        "Abstract": ["Abstract", "abstract", "summary", "basic_info_abstract"],
        "Author Keywords": ["Author Keywords", "author_keywords", "Keywords"],
        "Keywords Plus": ["Keywords Plus", "keywords_plus"],
        "UT (Unique ID)": ["UT (Unique ID)", "UT (Unique WOS ID)", "ut_unique_id", "ut_unique_wos_id", "ut", "UT", "UT号", "Accession Number", "accession_number"],
        "Search Direction": ["Search Direction", "search_direction", "direction_name", "原始检索方向"],
        "Suggested Section": ["Suggested Section", "suggested_section", "原始建议章节"],
        "Direction Index": ["Direction Index", "direction_index"],
        "Matched Direction Count": ["Matched Direction Count", "matched_direction_count"],
        "主题匹配度评分": ["主题匹配度评分"],
        "证据可用性评分": ["证据可用性评分"],
        "章节适配度评分": ["章节适配度评分"],
        "对象方法适配度评分": ["对象方法适配度评分"],
        "主题相关性总分": ["主题相关性总分", "topic_relevance_score", "relevance_score"],
        "相关性等级": ["相关性等级", "relevance_level"],
        "主题相关性理由": ["主题相关性理由"],
        "纳入建议": ["纳入建议", "inclusion_recommendation"],
        "排除或降权原因": ["排除或降权原因"],
        "相关性评分来源": ["relevance_score_source"],
        "Extraction Quality Status": ["extraction_quality_status"],
        "Extraction Quality Reason": ["extraction_quality_reason"],
        "Extraction Prompt Preview": ["extraction_prompt_preview"],
    }
    for target, candidates in export_aliases.items():
        source = first_existing_column(output, candidates)
        if not source:
            output[target] = ""
            continue
        output[target] = output[source].fillna("")
    for column in REVIEW_EVIDENCE_EXPORT_COLUMNS:
        if column not in output.columns:
            output[column] = ""
    preferred_order = list(export_aliases.keys()) + [
        column for column in REVIEW_EVIDENCE_EXPORT_COLUMNS if column not in export_aliases
    ]
    remaining_columns = [column for column in output.columns if column not in preferred_order]
    return output.loc[:, preferred_order + remaining_columns]


def _strip_excel_illegal_characters(value: Any) -> Any:
    if isinstance(value, str):
        return EXCEL_ILLEGAL_CHARACTERS_RE.sub("", value)
    return value


def _sanitize_table_for_excel(table: pd.DataFrame) -> pd.DataFrame:
    output = table.copy()
    for column in output.columns:
        if pd.api.types.is_object_dtype(output[column]) or pd.api.types.is_string_dtype(output[column]):
            output[column] = output[column].map(_strip_excel_illegal_characters)
    return output


def _truncate_excel_oversized_cells(table: pd.DataFrame) -> pd.DataFrame:
    output = table.copy()
    for column in output.columns:
        if not (pd.api.types.is_object_dtype(output[column]) or pd.api.types.is_string_dtype(output[column])):
            continue
        output[column] = output[column].map(
            lambda value: (
                value[: EXCEL_CELL_CHAR_LIMIT - 40] + "\n[TRUNCATED FOR EXCEL]"
                if isinstance(value, str) and len(value) > EXCEL_CELL_CHAR_LIMIT
                else value
            )
        )
    return output


def serialize_excel_table(table: pd.DataFrame) -> bytes:
    sanitized = _truncate_excel_oversized_cells(_sanitize_table_for_excel(table))
    output = io.BytesIO()
    sanitized.to_excel(output, index=False, sheet_name="results")
    return output.getvalue()


def _clean_filename(value: Any, fallback: str) -> str:
    text = str(value or "").strip()
    text = re.sub(r"[^A-Za-z0-9._-]+", "_", text).strip("._")
    return (text or fallback)[:120]


def _first_nonblank(row: pd.Series, columns: tuple[str, ...]) -> str:
    for column in columns:
        if column not in row.index:
            continue
        value = row.get(column)
        if isinstance(value, pd.Series):
            for item in value.tolist():
                text = str(item or "").strip()
                if text:
                    return text
            continue
        text = str(value or "").strip()
        if text:
            return text
    return ""


def _safe_fulltext_stem(row: pd.Series, ordinal: int, used: set[str]) -> str:
    ut = _first_nonblank(row, UT_FILENAME_COLUMNS)
    stem = _clean_filename(ut, f"paper_{ordinal:04d}")
    base = stem
    suffix = 2
    while stem.lower() in used:
        stem = f"{base}_{suffix}"
        suffix += 1
    used.add(stem.lower())
    return stem


def _canonical_section_heading(line: str) -> str:
    normalized = re.sub(r"[^a-z0-9 ]+", " ", str(line or "").strip().lower())
    normalized = re.sub(r"\s+", " ", normalized).strip()
    if len(normalized) > 80:
        return ""
    for canonical, aliases in SECTION_ALIASES:
        if normalized in aliases:
            return canonical
    return ""


def _format_paragraph(lines: list[str]) -> str:
    paragraph = " ".join(line.strip() for line in lines if line.strip())
    return re.sub(r"\s+", " ", paragraph).strip()


def build_structured_fulltext_markdown(content_text: str, metadata: dict[str, Any] | None = None) -> tuple[str, str]:
    text = str(content_text or "").replace("\r", "\n")
    lines = [re.sub(r"\s+", " ", line).strip() for line in text.splitlines()]
    lines = [line for line in lines if line]
    title = str((metadata or {}).get("article_title") or "").strip()
    output: list[str] = []
    status = "unstructured_fallback"
    if title:
        output.extend(["#title", title, ""])
    paragraph_lines: list[str] = []
    section_hits = 0
    seen_sections: set[str] = set()

    def flush_paragraph() -> None:
        paragraph = _format_paragraph(paragraph_lines)
        paragraph_lines.clear()
        if paragraph:
            output.append(paragraph)
            output.append("")

    for line in lines:
        heading = _canonical_section_heading(line)
        if heading:
            flush_paragraph()
            if heading == "title" and title:
                continue
            output.append(f"#{heading}")
            output.append("")
            if heading not in seen_sections:
                section_hits += 1
                seen_sections.add(heading)
            continue
        paragraph_lines.append(line)
    flush_paragraph()
    if section_hits == 0:
        body = "\n".join(lines).strip()
        output = []
        if title:
            output.extend(["#title", title, ""])
        output.extend(["#full_text", body])
    elif section_hits == 1:
        status = "partially_structured"
    else:
        status = "structured"
    markdown = re.sub(r"\n{3,}", "\n\n", "\n".join(output).strip())
    return markdown, status


def _looks_like_fulltext_artifact_path(value: Any) -> bool:
    text = str(value or "").replace("\\", "/")
    return text.startswith(f"{FULLTEXT_DIR}/") and text.lower().endswith((".txt", ".md"))


def _externalize_content_text(table: pd.DataFrame) -> tuple[pd.DataFrame, list[tuple[str, str]]]:
    output = table.copy()
    artifacts: list[tuple[str, str]] = []
    if "content_text" not in output.columns:
        return output, artifacts
    for column in ("content_text_file", "content_text_excel_note", "content_text_structure_status"):
        if column not in output.columns:
            output[column] = ""
        output[column] = output[column].astype(object)
    used: set[str] = set()
    for ordinal, (index, value) in enumerate(output["content_text"].items(), start=1):
        if not isinstance(value, str) or not value.strip() or _looks_like_fulltext_artifact_path(value):
            continue
        row = output.loc[index]
        stem = _safe_fulltext_stem(row, ordinal, used)
        filename = f"{FULLTEXT_DIR}/{stem}.md"
        markdown, status = build_structured_fulltext_markdown(
            value,
            metadata={"article_title": row.get("Article Title", "") or row.get("article_title", "") or row.get("title", "")},
        )
        artifacts.append((filename, markdown))
        output.at[index, "content_text_file"] = filename
        output.at[index, "content_text"] = filename
        output.at[index, "content_text_structure_status"] = status
        output.at[index, "content_text_excel_note"] = "全文已外置到 ZIP 内的 Markdown 文件，避免 Excel 单元格过长。"
    return output, artifacts


def serialize_result_table(table: pd.DataFrame) -> tuple[bytes, pd.DataFrame, str, int]:
    output = table.copy()
    if "parsed_json" in output.columns:
        output = flatten_json_column(output, "parsed_json")
        output = output.drop(columns=["parsed_json"])
    packaged, artifacts = _externalize_content_text(prepare_export_table(output))
    excel_bytes = serialize_excel_table(packaged)
    if not artifacts:
        return excel_bytes, packaged, "xlsx", 0
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("manifest.xlsx", excel_bytes)
        archive.writestr(
            "README.txt",
            (
                "Open manifest.xlsx for the table.\n"
                "Rows with acquired full text store content_text as a relative Markdown path under fulltext/.\n"
                "Uploading this ZIP back into wos-review restores content_text from those Markdown files automatically.\n"
            ),
        )
        for filename, markdown in artifacts:
            archive.writestr(filename, markdown)
    return buf.getvalue(), packaged, "zip", len(artifacts)
