from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import re


UNICODE_DASHES_RE = re.compile(r"[‐‑‒–—―−－]")
BOOLEAN_AT_RE = re.compile(r"\s+(?:AND|OR|NOT)\b", flags=re.IGNORECASE)
YEAR_RE = re.compile(r"\d{4}")
YEAR_RANGE_RE = re.compile(r"(?P<start>\d{4})\s*-\s*(?P<end>\d{4})|(?P<year>\d{4})")
WOS_FIELD_TAGS = {
    "AB", "AD", "AI", "AK", "ALL", "AU", "CF", "CI", "CU", "DO", "DOP", "DT", "ED",
    "FD", "FG", "FO", "FPY", "FT", "GP", "IS", "KP", "OG", "OO", "PMID", "PS",
    "PUBL", "PY", "SA", "SDG", "SG", "SO", "SU", "TI", "TMAC", "TMIC", "TMSO",
    "TS", "UT", "WC", "ZP",
}
EXPLICIT_BINARY_OPERATORS = {"AND", "OR", "SAME"}
TEXT_EXPRESSION_FIELDS = {"TS", "TI", "AB", "AK", "KP", "FT"}
ATOMIC_VALUE_FIELDS = WOS_FIELD_TAGS - TEXT_EXPRESSION_FIELDS


@dataclass
class QueryNode:
    has_and: bool = False
    has_or: bool = False


@dataclass
class QueryToken:
    kind: str
    value: str
    start: int
    end: int


def normalize_wos_query_text(value: str) -> str:
    query = str(value or "").strip()
    if not query:
        return ""
    translation = str.maketrans(
        {
            "（": "(", "）": ")", "［": "[", "］": "]",
            "｛": "{", "｝": "}", "，": ",", "；": ";",
            "：": ":", "＝": "=", "“": '"', "”": '"',
            "‘": "'", "’": "'",
        }
    )
    query = query.translate(translation)
    return UNICODE_DASHES_RE.sub("-", query)


def clean_search_query(value: str) -> str:
    query = normalize_wos_query_text(value)
    query = normalize_wos_publication_year_filters(query)
    return re.sub(r"\s+", " ", query.strip())


def auto_fix_wos_search_query(value: str) -> str:
    query = clean_search_query(value)
    if not query:
        return query
    query = _auto_quote_multiword_near_operands(query)
    query = _auto_expand_group_near_group(query)
    return clean_search_query(query)


def validate_wos_search_query(value: str) -> str:
    query = auto_fix_wos_search_query(value)
    if not query:
        raise ValueError("检索式为空。")
    _validate_balanced_parentheses(query)
    _validate_balanced_quotes(query)
    tokens = _tokenize_wos_query(query)
    if not tokens:
        raise ValueError("检索式为空。")
    _validate_token_sequence(tokens)
    _validate_field_usage(tokens)
    _parse_query_tokens(tokens)
    return query


def _validate_balanced_parentheses(query: str) -> None:
    depth = 0
    for char in query:
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth < 0:
                raise ValueError("检索式括号不配平：存在多余的右括号。")
    if depth != 0:
        raise ValueError("检索式括号不配平：缺少右括号。")


def _validate_balanced_quotes(query: str) -> None:
    in_quote = False
    quote_char = ""
    for char in query:
        if char in {'"', "'"}:
            if in_quote and char == quote_char:
                in_quote = False
                quote_char = ""
            elif not in_quote:
                in_quote = True
                quote_char = char
    if in_quote:
        raise ValueError("检索式引号不配平。")


def _tokenize_wos_query(query: str) -> list[QueryToken]:
    tokens: list[QueryToken] = []
    idx = 0
    while idx < len(query):
        char = query[idx]
        if char.isspace():
            idx += 1
            continue
        if char in "()":
            tokens.append(QueryToken(char, char, idx, idx + 1))
            idx += 1
            continue
        if char in {'"', "'"}:
            end = idx + 1
            while end < len(query) and query[end] != char:
                end += 1
            if end >= len(query):
                raise ValueError("检索式引号不配平。")
            end += 1
            tokens.append(QueryToken("TERM", query[idx:end], idx, end))
            idx = end
            continue
        if char.isalpha():
            field_match = re.match(r"[A-Za-z][A-Za-z0-9]*\s*=", query[idx:])
            if field_match:
                raw = field_match.group(0)
                field = raw.split("=", 1)[0].strip().upper()
                tokens.append(QueryToken("FIELD", field, idx, idx + len(raw)))
                idx += len(raw)
                continue
            near_match = re.match(r"(?i)(NEAR|SAME)\s*/\s*\d+", query[idx:])
            if near_match:
                op = near_match.group(1).upper()
                tokens.append(QueryToken("OP", op, idx, idx + len(near_match.group(0))))
                idx += len(near_match.group(0))
                continue
            word_match = re.match(r"[A-Za-z][A-Za-z0-9_-]*", query[idx:])
            assert word_match is not None
            word = word_match.group(0)
            upper_word = word.upper()
            kind = "OP" if upper_word in {"AND", "OR", "NOT"} else "TERM"
            tokens.append(QueryToken(kind, upper_word if kind == "OP" else word, idx, idx + len(word)))
            idx += len(word)
            continue
        end = idx + 1
        while end < len(query) and not query[end].isspace() and query[end] not in "()":
            end += 1
        tokens.append(QueryToken("TERM", query[idx:end], idx, end))
        idx = end
    return tokens


def _validate_token_sequence(tokens: list[QueryToken]) -> None:
    for index, token in enumerate(tokens):
        prev_token = tokens[index - 1] if index > 0 else None
        next_token = tokens[index + 1] if index + 1 < len(tokens) else None
        if token.kind == "OP":
            if token.value in EXPLICIT_BINARY_OPERATORS:
                if prev_token is None or prev_token.kind in {"OP", "("}:
                    raise ValueError(f"检索式中 {token.value} 左侧缺少有效表达式。")
                if next_token is None or next_token.kind in {"OP", ")"}:
                    raise ValueError(f"检索式中 {token.value} 右侧缺少有效表达式。")
            elif token.value == "NOT":
                if prev_token is None or prev_token.kind in {"OP", "("}:
                    raise ValueError("检索式中 NOT 左侧缺少有效表达式。")
                if next_token is None or next_token.kind in {"OP", ")"}:
                    raise ValueError("检索式中 NOT 右侧缺少有效表达式。")
        elif token.kind == "FIELD":
            if next_token is None or next_token.kind not in {"(", "TERM", "FIELD"}:
                raise ValueError(f"字段 {token.value}= 后缺少检索内容。")
        elif token.kind == "(" and next_token is not None and next_token.kind == ")":
            raise ValueError("检索式中存在空括号。")


def _validate_field_usage(tokens: list[QueryToken]) -> None:
    for token in tokens:
        if token.kind == "FIELD" and token.value not in WOS_FIELD_TAGS:
            raise ValueError(f"检索式中存在未知字段标签：{token.value}=")


def _parse_query_tokens(tokens: list[QueryToken]) -> QueryNode:
    position = 0

    def merge_nodes(left: QueryNode, right: QueryNode, *, op: str) -> QueryNode:
        return QueryNode(
            has_and=left.has_and or right.has_and or op in {"AND", "SAME"},
            has_or=left.has_or or right.has_or or op == "OR",
        )

    def parse_expression(stop_kinds: set[str] | None = None) -> QueryNode:
        return parse_or(stop_kinds)

    def parse_or(stop_kinds: set[str] | None = None) -> QueryNode:
        nonlocal position
        node = parse_and(stop_kinds)
        while position < len(tokens):
            token = tokens[position]
            if stop_kinds and token.kind in stop_kinds:
                break
            if token.kind != "OP" or token.value != "OR":
                break
            position += 1
            right = parse_and(stop_kinds)
            node = merge_nodes(node, right, op="OR")
        return node

    def parse_and(stop_kinds: set[str] | None = None) -> QueryNode:
        nonlocal position
        node = parse_near(stop_kinds)
        while position < len(tokens):
            token = tokens[position]
            if stop_kinds and token.kind in stop_kinds:
                break
            if token.kind != "OP" or token.value != "AND":
                break
            position += 1
            right = parse_near(stop_kinds)
            node = merge_nodes(node, right, op="AND")
        return node

    def parse_near(stop_kinds: set[str] | None = None) -> QueryNode:
        nonlocal position
        node = parse_term(stop_kinds)
        while position < len(tokens):
            token = tokens[position]
            if stop_kinds and token.kind in stop_kinds:
                break
            if token.kind != "OP" or token.value not in {"SAME", "NEAR"}:
                break
            op = token.value
            position += 1
            right = parse_term(stop_kinds)
            if node.has_and or right.has_and:
                raise ValueError(
                    "检索式中 NEAR/SAME 不能直接连接会形成隐式 AND 的复杂表达式。"
                    " 请改写为逐对邻近，或改用普通 AND。"
                )
            node = merge_nodes(node, right, op=op)
        return node

    def parse_term(stop_kinds: set[str] | None = None) -> QueryNode:
        nonlocal position
        token = tokens[position] if position < len(tokens) else None
        if token is None:
            raise ValueError("检索式不完整。")
        if token.kind == "OP" and token.value == "NOT":
            position += 1
            return parse_term(stop_kinds)
        return parse_factor(stop_kinds)

    def parse_factor(stop_kinds: set[str] | None = None) -> QueryNode:
        nonlocal position
        token = tokens[position] if position < len(tokens) else None
        if token is None:
            raise ValueError("检索式不完整。")
        if token.kind == "FIELD":
            field = token.value
            position += 1
            child = parse_field_value(field)
            _validate_field_content(field, child, token)
            return child
        if token.kind == "(":
            position += 1
            node = parse_expression({")"})
            if position >= len(tokens) or tokens[position].kind != ")":
                raise ValueError("检索式括号不配平：缺少右括号。")
            position += 1
            return node
        if token.kind == "TERM":
            position += 1
            return QueryNode(has_and=False, has_or=False)
        raise ValueError(f"检索式中位置 {token.start + 1} 附近存在非法结构。")

    def parse_field_value(field: str) -> QueryNode:
        nonlocal position
        token = tokens[position] if position < len(tokens) else None
        if token is None:
            raise ValueError(f"字段 {field}= 后缺少检索内容。")
        if field in ATOMIC_VALUE_FIELDS:
            if token.kind == "(":
                position += 1
                depth = 1
                has_content = False
                while position < len(tokens) and depth > 0:
                    current = tokens[position]
                    if current.kind == "(":
                        depth += 1
                    elif current.kind == ")":
                        depth -= 1
                        if depth == 0:
                            position += 1
                            break
                    else:
                        has_content = True
                    position += 1
                if depth != 0:
                    raise ValueError("检索式括号不配平：缺少右括号。")
                if not has_content:
                    raise ValueError("检索式中存在空括号。")
                return QueryNode(has_and=False, has_or=False)
        if field in TEXT_EXPRESSION_FIELDS and token.kind == "(":
            start = position
            end = _find_closing_paren_token(tokens, start)
            if end == -1:
                raise ValueError("检索式括号不配平：缺少右括号。")
            inner_tokens = _tokens_with_implicit_and(tokens[start + 1 : end])
            position = end + 1
            return _parse_query_tokens(inner_tokens)
        if token.kind == "(":
            position += 1
            node = parse_expression({")"})
            if position >= len(tokens) or tokens[position].kind != ")":
                raise ValueError("检索式括号不配平：缺少右括号。")
            position += 1
            return node
        return parse_factor()

    root = parse_expression()
    if position != len(tokens):
        token = tokens[position]
        raise ValueError(f"检索式中位置 {token.start + 1} 附近存在未解析内容。")
    return root


def _validate_field_content(field: str, node: QueryNode, token: QueryToken) -> None:
    _ = (field, node, token)


def _find_closing_paren_token(tokens: list[QueryToken], start: int) -> int:
    depth = 0
    for index in range(start, len(tokens)):
        token = tokens[index]
        if token.kind == "(":
            depth += 1
        elif token.kind == ")":
            depth -= 1
            if depth == 0:
                return index
    return -1


def _tokens_with_implicit_and(tokens: list[QueryToken]) -> list[QueryToken]:
    if not tokens:
        raise ValueError("检索式中存在空括号。")
    result: list[QueryToken] = []
    prev_term_like = False
    for token in tokens:
        current_term_like = token.kind in {"TERM", "FIELD", "("}
        if prev_term_like and current_term_like:
            result.append(QueryToken("OP", "AND", token.start, token.start))
        result.append(token)
        prev_term_like = token.kind in {"TERM", ")"}
    return result


def _auto_quote_multiword_near_operands(query: str) -> str:
    pattern = re.compile(r"\b(NEAR|SAME)\s*/\s*(\d+)\s+([A-Za-z][A-Za-z0-9_-]*(?:\s+[A-Za-z][A-Za-z0-9_-]*)+)")
    prev = None
    while prev != query:
        prev = query
        query = pattern.sub(lambda m: f"{m.group(1)}/{m.group(2)} \"{m.group(3)}\"", query)
    return query


def _auto_expand_group_near_group(query: str) -> str:
    pattern = re.compile(r"(?P<left>\((?:[^()\"]|\"[^\"]*\")+\))\s*(?P<op>NEAR|SAME)\s*/\s*(?P<n>\d+)\s*(?P<right>\((?:[^()\"]|\"[^\"]*\")+\))", re.IGNORECASE)
    prev = None
    while prev != query:
        prev = query
        query = pattern.sub(_expand_group_near_group_match, query)
    return query


def _expand_group_near_group_match(match: re.Match[str]) -> str:
    left_group = match.group("left")
    right_group = match.group("right")
    op = match.group("op").upper()
    distance = match.group("n")
    left_terms = _split_top_level_or_terms(left_group[1:-1])
    right_terms = _split_top_level_or_terms(right_group[1:-1])
    if len(left_terms) <= 1 or len(right_terms) <= 1:
        return match.group(0)
    pairs: list[str] = []
    for left in left_terms:
        fixed_left = _quote_if_multiword(left)
        for right in right_terms:
            fixed_right = _quote_if_multiword(right)
            pairs.append(f"({fixed_left} {op}/{distance} {fixed_right})")
    return "(" + " OR ".join(pairs) + ")"


def _split_top_level_or_terms(text: str) -> list[str]:
    parts: list[str] = []
    current: list[str] = []
    depth = 0
    in_quote = False
    quote_char = ""
    idx = 0
    while idx < len(text):
        char = text[idx]
        if char in {'"', "'"}:
            if in_quote and char == quote_char:
                in_quote = False
                quote_char = ""
            elif not in_quote:
                in_quote = True
                quote_char = char
            current.append(char)
            idx += 1
            continue
        if not in_quote:
            if char == "(":
                depth += 1
            elif char == ")":
                depth = max(0, depth - 1)
            elif depth == 0 and text[idx: idx + 2].upper() == "OR":
                before_ok = idx == 0 or text[idx - 1].isspace()
                after_idx = idx + 2
                after_ok = after_idx >= len(text) or text[after_idx].isspace()
                if before_ok and after_ok:
                    part = "".join(current).strip()
                    if part:
                        parts.append(part)
                    current = []
                    idx = after_idx
                    continue
        current.append(char)
        idx += 1
    tail = "".join(current).strip()
    if tail:
        parts.append(tail)
    return parts


def _quote_if_multiword(term: str) -> str:
    text = term.strip()
    if not text or text.startswith('"') or text.startswith("'"):
        return text
    if re.search(r"\s", text):
        return f'"{text}"'
    return text


def normalize_wos_publication_year_filters(query: str) -> str:
    if not query:
        return ""
    result: list[str] = []
    cursor = 0
    for match in re.finditer(r"\bPY\s*=\s*", query, flags=re.IGNORECASE):
        result.append(query[cursor : match.start()])
        cursor = match.end()
        while cursor < len(query) and query[cursor].isspace():
            cursor += 1
        if cursor < len(query) and query[cursor] == "(":
            end = find_matching_parenthesis(query, cursor)
            if end == -1:
                raise ValueError("Invalid PY year format: missing closing parenthesis.")
            raw_years = query[cursor + 1 : end]
            cursor = end + 1
        else:
            end = find_bare_py_value_end(query, cursor)
            raw_years = query[cursor:end]
            cursor = end
        result.append(f"PY=({normalize_py_year_value(raw_years)})")
    result.append(query[cursor:])
    return "".join(result)


def find_matching_parenthesis(text: str, start: int) -> int:
    depth = 0
    for index in range(start, len(text)):
        char = text[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index
    return -1


def find_opening_parenthesis(text: str, start: int) -> int:
    depth = 0
    for index in range(start, -1, -1):
        char = text[index]
        if char == ")":
            depth += 1
        elif char == "(":
            depth -= 1
            if depth == 0:
                return index
    return -1


def find_bare_py_value_end(text: str, start: int) -> int:
    index = start
    while index < len(text):
        if text[index] == ")":
            break
        boolean_match = BOOLEAN_AT_RE.match(text, index)
        if boolean_match:
            next_index = boolean_match.end()
            while next_index < len(text) and text[next_index].isspace():
                next_index += 1
            if not re.match(r"\d{4}", text[next_index:]):
                break
        index += 1
    return index


def normalize_py_year_value(value: str) -> str:
    text = normalize_wos_query_text(value).strip().strip("\"'")
    text = re.sub(r"\s+", " ", text)
    if not text:
        raise ValueError("Invalid PY year format: publication year cannot be empty.")
    relative = normalize_relative_year_value(text)
    if relative:
        return relative
    if not YEAR_RE.search(text):
        raise ValueError("Invalid PY year format.")
    tokens: list[tuple[str, int, int]] = []
    seen: set[str] = set()
    for match in YEAR_RANGE_RE.finditer(text):
        if match.group("year"):
            year = int(match.group("year"))
            token = f"{year:04d}"
            start_year = end_year = year
        else:
            start_year = int(match.group("start"))
            end_year = int(match.group("end"))
            if start_year > end_year:
                start_year, end_year = end_year, start_year
            token = f"{start_year:04d}-{end_year:04d}"
        if token in seen:
            continue
        seen.add(token)
        tokens.append((token, start_year, end_year))
    if not tokens:
        raise ValueError("Invalid PY year format.")
    if all(start == end for _, start, end in tokens):
        years = sorted({start for _, start, _ in tokens})
        if len(years) == 1:
            return f"{years[0]:04d}"
        if years == list(range(years[0], years[-1] + 1)):
            return f"{years[0]:04d}-{years[-1]:04d}"
        return " OR ".join(f"{year:04d}" for year in years)
    return " OR ".join(token for token, _, _ in tokens)


def normalize_relative_year_value(value: str) -> str:
    text = value.strip().lower()
    match = re.search(r"\b(?:last|past|recent|latest)\s+(\d{1,2})\s+years?\b", text)
    if not match:
        match = re.search(r"(?:近|最近|过去|近来|近年)\s*(\d{1,2})\s*年", value)
    if not match:
        return ""
    count = max(1, min(int(match.group(1)), 50))
    current_year = datetime.now().year
    return f"{current_year - count + 1:04d}-{current_year:04d}"
