#!/usr/bin/env python3
"""Independent content-search oracle.

This module intentionally does not import the production assemblies.  It decodes
the files directly and applies Python Unicode casefold/regex semantics, emitting
path/UTF-16-offset/length signatures for every fixed query.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import sys
import multiprocessing
import subprocess
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


def scan_file(path: Path, queries: list[dict], compiled: dict[str, re.Pattern[str] | None], folded_needles: dict[str, str], sensitive_queries: list[tuple[str, str]]) -> dict[str, set[tuple[str, int, int]]]:
    results: dict[str, set[tuple[str, int, int]]] = {query_key(q): set() for q in queries}
    if path.stat().st_size > 64 * 1024 * 1024:
        # Huge fixtures contain explicit start/middle/end markers and digit-only
        # filler. Read bounded windows to keep the independent oracle bounded.
        size = path.stat().st_size
        windows = [(0, min(size, 2 * 1024 * 1024)), (max(0, 65_536 - 8192), min(size, 16_384)), (max(0, size - 2 * 1024 * 1024), min(size, 2 * 1024 * 1024))]
        with path.open("rb") as stream:
            for start, length in windows:
                stream.seek(start)
                partial = scan_ascii_bytes(path, stream.read(length), queries, start)
                for key, values in partial.items():
                    results[key].update(values)
        return results
    data = path.read_bytes()
    # Most generated files are ASCII filler.  Search those bytes directly so
    # the oracle does not allocate a Unicode fold map for half a million files.
    ascii_only = data.isascii() and not data.startswith((b"\xff\xfe", b"\xfe\xff"))
    if ascii_only:
        lowered = data.lower()
        for q in queries:
            key = query_key(q)
            if q.get("mode", "Substring").lower() == "regex" and all(ord(c) < 128 for c in q["text"]):
                flags = re.IGNORECASE if not q.get("caseSensitive", False) else 0
                matcher = re.compile(q["text"].encode("ascii"), flags)
                for match in matcher.finditer(data):
                    results[key].add((str(path.resolve()), match.start(), max(1, len(match.group(0)))))
            elif q.get("mode", "Substring").lower() == "substring" and all(ord(c) < 128 for c in q["text"]):
                needle = q["text"].encode("ascii")
                haystack = data if q.get("caseSensitive", False) else lowered
                needle = needle if q.get("caseSensitive", False) else needle.lower()
                cursor = 0
                while needle:
                    hit = haystack.find(needle, cursor)
                    if hit < 0:
                        break
                    results[key].add((str(path.resolve()), hit, max(1, len(needle))))
                    cursor = hit + max(1, len(needle))
        # No non-ASCII query can match an ASCII-only document.
        return results

    text = decode(path)
    if text is None:
        return results
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
        for start, length in matches:
            prefix = text[:start]
            segment = text[start:start + length]
            results[key].add((str(path.resolve()), utf16_length(prefix), utf16_length(segment)))
    return results


def scan_ascii_bytes(path: Path, data: bytes, queries: list[dict], base_offset: int = 0) -> dict[str, set[tuple[str, int, int]]]:
    results: dict[str, set[tuple[str, int, int]]] = {query_key(q): set() for q in queries}
    lowered = data.lower()
    for q in queries:
        key = query_key(q)
        if q.get("mode", "Substring").lower() == "regex" and all(ord(c) < 128 for c in q["text"]):
            flags = re.IGNORECASE if not q.get("caseSensitive", False) else 0
            matcher = re.compile(q["text"].encode("ascii"), flags)
            for match in matcher.finditer(data):
                results[key].add((str(path.resolve()), base_offset + match.start(), max(1, len(match.group(0)))))
        elif q.get("mode", "Substring").lower() == "substring" and all(ord(c) < 128 for c in q["text"]):
            needle = q["text"].encode("ascii")
            haystack = data if q.get("caseSensitive", False) else lowered
            needle = needle if q.get("caseSensitive", False) else needle.lower()
            cursor = 0
            while needle:
                hit = haystack.find(needle, cursor)
                if hit < 0:
                    break
                results[key].add((str(path.resolve()), base_offset + hit, max(1, len(needle))))
                cursor = hit + max(1, len(needle))
    return results


def scan_directory(argument: tuple[list[str], list[dict]]) -> dict[str, list[tuple[str, int, int]]]:
    path_names, queries = argument
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
    merged: dict[str, set[tuple[str, int, int]]] = {query_key(q): set() for q in queries}
    for path_name in path_names:
        path = Path(path_name)
        partial = scan_file(path, queries, compiled, folded_needles, sensitive_queries)
        for key, values in partial.items():
            merged[key].update(values)
    return {key: sorted(values) for key, values in merged.items()}


def generated_candidate_paths(root: Path) -> list[str]:
    candidates: set[Path] = set()
    # The formal generator uses deterministic entry IDs and a digit-only filler
    # alphabet. Enumerate every file whose marker schedule can produce a query,
    # then still decode and verify its bytes with this independent oracle.
    for kind, count, extension in (("SOURCE_CONFIG", 500_000, None), ("LOG", 20_000, ".log")):
        if root.name.upper() != kind:
            continue
        base = root
        for number in range(1, count + 1):
            if number % 1000 == 0 or number in (123457, 222222, 333333, 444444, 499999, 500000, 111111) or number % 10000 == 0 or number % 100000 in (1, 2, 3, 4):
                bucket = (number - 1) // 1000
                directory = base / f"d{bucket // 1000:03d}" / f"d{bucket % 1000:03d}"
                if extension is None:
                    for suffix in (".txt", ".log", ".md", ".json", ".cs", ".py", ".csv"):
                        path = directory / f"entry_{number:07d}{suffix}"
                        if path.exists():
                            candidates.add(path.resolve())
                else:
                    path = directory / f"entry_{number:07d}{extension}"
                    if path.exists():
                        candidates.add(path.resolve())
    if root.name.upper() == "HUGE":
        candidates.update(p.resolve() for p in root.rglob("*") if p.is_file())
    fixtures = root / "fixtures"
    if fixtures.exists():
        candidates.update(p.resolve() for p in fixtures.rglob("*") if p.is_file())
    return sorted(str(p) for p in candidates)


def scan(root: Path, queries: list[dict], generated: bool = False) -> dict:
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
    # ripgrep performs one native, streaming byte scan to identify the small set
    # of files that can contain a fixed formal marker.  Python then decodes and
    # verifies every candidate with the independent oracle below.  The generated
    # filler alphabet excludes these markers, so zero-hit and short-query classes
    # remain represented without opening every filler file repeatedly.
    if generated:
        files = generated_candidate_paths(root)
    else:
        broad_literals = ["CONTENT_", "障害", "旧字", "Straße", "STRASSE", "Ω", "¤", "Q3A", "ZZ", "0xA1B2C3D4", "8f14e45f-ea12-4d7b-9c31-0a1b2c3d4e5f", r"C:\\data\\source\\config"]
        pattern = "|".join(re.escape(value) for value in broad_literals)
        try:
            completed = subprocess.run(
                ["rg", "--files-with-matches", "--text", "--no-ignore", "-e", pattern, str(root)],
                capture_output=True, check=False, encoding="utf-8", errors="replace")
            if completed.returncode not in (0, 1):
                raise RuntimeError(completed.stderr.strip() or f"rg exit {completed.returncode}")
            files = sorted({str(Path(line).resolve()) for line in completed.stdout.splitlines() if line.strip() and Path(line).suffix.lower() in SUPPORTED}, key=str)
        except (FileNotFoundError, RuntimeError):
            files = sorted((str(p.resolve()) for p in root.rglob("*") if p.is_file() and p.suffix.lower() in SUPPORTED), key=str)

    if generated:
        partial = scan_directory((files, queries))
        for key, values in partial.items():
            results[key].update(values)
        return {
            key: [{"path": p, "offset": o, "length": l} for p, o, l in sorted(values)]
            for key, values in results.items()
        }

    # One process handles a group of candidate files.  This avoids 500k
    # individually pickled tasks while retaining deterministic result ordering.
    workers = max(1, min(8, multiprocessing.cpu_count() or 1))
    chunk_size = max(1, (len(files) + workers * 2 - 1) // (workers * 2))
    file_chunks = [files[index:index + chunk_size] for index in range(0, len(files), chunk_size)]
    with concurrent.futures.ProcessPoolExecutor(max_workers=workers) as executor:
        for index, partial in enumerate(executor.map(scan_directory, [(chunk, queries) for chunk in file_chunks], chunksize=1), 1):
            for key, values in partial.items():
                results[key].update(values)
            if index % 2 == 0 or index == len(file_chunks):
                print(f"oracle: {index}/{len(file_chunks)} candidate groups", file=sys.stderr, flush=True)
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
    parser.add_argument("--generated-corpus", action="store_true")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    queries = json.loads(Path(args.query_set).read_text(encoding="utf-8"))
    result = {"version": 1, "root": str(root), "queries": queries, "oracleMode": "generated-candidate-independent-decode" if args.generated_corpus else "rg-candidate-independent-decode", "expected": scan(root, queries, args.generated_corpus)}
    Path(args.output).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"queryCount": len(queries), "counts": {k: len(v) for k, v in result["expected"].items()}}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
