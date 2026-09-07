#!/usr/bin/env python3
"""Independent content-search oracle.

This module intentionally does not import the production assemblies.  It decodes
the files directly and applies Python Unicode casefold/regex semantics, emitting
path/UTF-16-offset/length signatures for every fixed query.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
import unicodedata
from pathlib import Path

SUPPORTED = {".txt", ".log", ".md", ".csv", ".json", ".xml", ".yaml", ".yml", ".ini", ".cfg", ".conf", ".cs", ".c", ".h", ".cpp", ".hpp", ".cc", ".py", ".js", ".jsx", ".ts", ".tsx", ".java", ".rs", ".go", ".ps1", ".bat", ".cmd", ".sql"}


def decode(path: Path) -> str | None:
    data = path.read_bytes()
    if data.startswith(b"\xef\xbb\xbf"):
        return data.decode("utf-8-sig")
    if data.startswith(b"\xff\xfe"):
        return data[2:].decode("utf-16-le")
    if data.startswith(b"\xfe\xff"):
        return data[2:].decode("utf-16-be")
    if b"\x00" in data[:4096] and data[:4096].count(b"\x00") > max(1, len(data[:4096]) // 32):
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        try:
            return data.decode("cp932")
        except UnicodeDecodeError:
            return None


def utf16_length(value: str) -> int:
    return len(value.encode("utf-16-le")) // 2


def normalize_folded(value: str) -> str:
    return unicodedata.normalize("NFC", value).casefold()


def folded_matches(text: str, query: str) -> list[tuple[int, int]]:
    folded: list[str] = []
    mapping: list[int] = []
    for index, char in enumerate(text):
        part = normalize_folded(char)
        folded.append(part)
        mapping.extend([index] * len(part))
    haystack = "".join(folded)
    needle = normalize_folded(query)
    if not needle:
        return []
    result: list[tuple[int, int]] = []
    start = 0
    while True:
        hit = haystack.find(needle, start)
        if hit < 0:
            break
        original_start = mapping[hit]
        original_end = mapping[min(len(mapping) - 1, hit + len(needle) - 1)]
        result.append((original_start, max(1, original_end - original_start + 1)))
        start = hit + max(1, len(needle))
    return result


def query_key(query: dict) -> str:
    return f"{query.get('mode', 'Substring')}|case={bool(query.get('caseSensitive', False))}|{query.get('text', '')}"


def signature(path: Path, offset: int, length: int) -> dict:
    # C# DecodedCharOffset is a UTF-16 string index.  Formal markers are BMP,
    # but calculating the prefix explicitly keeps the oracle correct for them.
    return {"path": str(path.resolve()), "offset": offset, "length": length}


def scan(root: Path, queries: list[dict]) -> dict:
    results: dict[str, set[tuple[str, int, int]]] = {query_key(q): set() for q in queries}
    compiled: dict[str, re.Pattern[str] | None] = {}
    folded_needles: dict[str, str] = {}
    sensitive_queries: list[tuple[str, str]] = []
    for q in queries:
        key = query_key(q)
        if q.get("mode", "Substring").lower() == "regex":
            flags = re.IGNORECASE if not q.get("caseSensitive", False) else 0
            compiled[key] = re.compile(q["text"], flags)
        elif q.get("caseSensitive", False):
            sensitive_queries.append((key, q["text"]))
        else:
            folded_needles[key] = normalize_folded(q["text"])
    files = sorted((p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in SUPPORTED), key=lambda p: str(p))
    for number, path in enumerate(files, 1):
        text = decode(path)
        if text is None:
            continue
        folded_text = ""
        folded_map: list[int] = []
        if folded_needles:
            folded_parts: list[str] = []
            for index, char in enumerate(text):
                part = normalize_folded(char)
                folded_parts.append(part)
                folded_map.extend([index] * len(part))
            folded_text = "".join(folded_parts)
        for q in queries:
            key = query_key(q)
            if q.get("mode", "Substring").lower() == "regex":
                matcher = compiled[key]
                assert matcher is not None
                matches = [(m.start(), max(1, len(m.group(0)))) for m in matcher.finditer(text)]
            elif q.get("caseSensitive", False):
                needle = q["text"]
                matches = []
                cursor = 0
                while needle:
                    hit = text.find(needle, cursor)
                    if hit < 0:
                        break
                    matches.append((hit, max(1, len(needle))))
                    cursor = hit + max(1, len(needle))
            else:
                needle = folded_needles[key]
                matches = []
                cursor = 0
                while needle:
                    hit = folded_text.find(needle, cursor)
                    if hit < 0:
                        break
                    original_start = folded_map[hit]
                    original_end = folded_map[min(len(folded_map) - 1, hit + len(needle) - 1)]
                    matches.append((original_start, max(1, original_end - original_start + 1)))
                    cursor = hit + max(1, len(needle))
            # C# uses UTF-16 offsets.  Convert the original Python code-point
            # index and length to UTF-16 units for every returned signature.
            for start, length in matches:
                prefix = text[:start]
                segment = text[start:start + length]
                results[key].add((str(path.resolve()), utf16_length(prefix), utf16_length(segment)))
        if number % 10000 == 0:
            print(f"oracle: {number}/{len(files)}", file=sys.stderr, flush=True)
    return {
        key: [{"path": p, "offset": o, "length": l} for p, o, l in sorted(values)]
        for key, values in results.items()
    }


def main() -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--query-set", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    root = Path(args.root).resolve()
    queries = json.loads(Path(args.query_set).read_text(encoding="utf-8"))
    result = {"version": 1, "root": str(root), "queries": queries, "expected": scan(root, queries)}
    Path(args.output).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"queryCount": len(queries), "counts": {k: len(v) for k, v in result["expected"].items()}}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
