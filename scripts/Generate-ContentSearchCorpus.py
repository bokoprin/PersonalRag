#!/usr/bin/env python3
"""Generate the real filesystem corpus used by the content-search bakeoff.

The generator deliberately writes ordinary files (no sparse or virtual metadata)
and emits a compact manifest.  Marker placement is deterministic from seed 123456;
the independent oracle scans the resulting files instead of consuming marker data.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import time
from pathlib import Path

SEED = 123456
SOURCE_FILES = 500_000
SOURCE_BYTES = 5 * 1024**3 + 128 * 1024**2
LOG_FILES = 20_000
LOG_BYTES = 20 * 1024**3 + 128 * 1024**2
HUGE_FILES = 20
HUGE_BYTES = 10 * 1024**3 + 128 * 1024**2

FILLER = (b"0123456789 0123456789 0123456789 0123456789\n" * 512)
SUPPORTED = (".txt", ".log", ".md", ".json", ".cs", ".py", ".csv")


def marker_list(kind: str, number: int) -> list[str]:
    markers: list[str] = []
    if number % 1000 == 0:
        markers.append("CONTENT_COMMON_KAPPA")
    if number == 123457:
        markers.append("CONTENT_RARE_KAPPA_9901")
    if number % 10000 == 0:
        markers.append("障害復旧")
    if number == 222222:
        markers.append("0xA1B2C3D4")
    if number == 333333:
        markers.append("8f14e45f-ea12-4d7b-9c31-0a1b2c3d4e5f")
    if number == 444444:
        markers.append(r"C:\data\source\config")
    if number % 100000 == 1:
        markers.append("Q3A")
    if number % 100000 == 2:
        markers.append("ZZ")
    if number % 100000 == 3:
        markers.append("¤")
    if number % 100000 == 4:
        markers.append("ΩΩΩ")
    if number == 111111:
        markers.append("CONTENT_CASE_SENSITIVE")
    if number == 499999:
        markers.append("CONTENT_DEEP_END")
    if number == 500000:
        markers.append("CONTENT_UPDATE_TARGET")
    if kind == "LOG" and number % 5000 == 0:
        markers.append("CONTENT_LOG_MIDDLE")
    if kind == "HUGE":
        markers.append("CONTENT_HUGE_START" if number == 1 else "CONTENT_HUGE_END")
        markers.append("CONTENT_BLOCK_BOUNDARY")
    return markers


def write_filler(stream, count: int) -> None:
    while count:
        chunk = FILLER[: min(count, len(FILLER))]
        stream.write(chunk)
        count -= len(chunk)


def write_marked(path: Path, size: int, markers: list[str], encoding: str = "utf-8") -> None:
    encoded = [m.encode(encoding) for m in markers]
    # Keep all marker bytes well inside the requested file and at stable positions.
    positions: list[tuple[int, bytes]] = []
    if encoded:
        stride = max(256, (size - 8192) // (len(encoded) + 1))
        for index, (marker, payload) in enumerate(zip(markers, encoded)):
            if marker == "CONTENT_HUGE_START":
                position = 1024
            elif marker == "CONTENT_HUGE_END":
                position = max(1024, size - len(payload) - 1024)
            elif marker == "CONTENT_BLOCK_BOUNDARY":
                position = min(size - len(payload) - 1, 65_536 - 8)
            elif marker == "CONTENT_DEEP_END":
                position = max(1024, size - len(payload) - 1024)
            else:
                position = min(size - len(payload) - 1, 1024 + stride * (index + 1))
            positions.append((position, payload))
    positions.sort()
    with path.open("wb", buffering=1024 * 1024) as stream:
        cursor = 0
        for position, payload in positions:
            if position < cursor:
                continue
            write_filler(stream, position - cursor)
            stream.write(payload)
            cursor = position + len(payload)
        write_filler(stream, size - cursor)


def write_fixture(path: Path, text: str, encoding: str, bom: bool = False) -> None:
    codec = {"utf8": "utf-8", "utf16le": "utf-16-le", "utf16be": "utf-16-be", "cp932": "cp932"}[encoding]
    payload = text.encode(codec)
    if bom:
        payload = {"utf8": b"\xef\xbb\xbf", "utf16le": b"\xff\xfe", "utf16be": b"\xfe\xff"}[encoding] + payload
    path.write_bytes(payload)


def generate_kind(root: Path, kind: str, count: int, total_bytes: int, ext: str) -> dict:
    directory = root / kind
    directory.mkdir(parents=True, exist_ok=True)
    per_file = total_bytes // count
    remainder = total_bytes - per_file * count
    started = time.monotonic()
    for number in range(1, count + 1):
        # Four levels keep paths realistic while remaining below Windows path limits.
        bucket = (number - 1) // 1000
        sub = directory / f"d{bucket // 1000:03d}" / f"d{bucket % 1000:03d}"
        sub.mkdir(parents=True, exist_ok=True)
        extension = SUPPORTED[(number - 1) % len(SUPPORTED)] if kind == "SOURCE_CONFIG" else ext
        path = sub / f"entry_{number:07d}{extension}"
        size = per_file + (1 if number <= remainder else 0)
        markers = marker_list(kind, number)
        write_marked(path, size, markers)
        if number % 5000 == 0 or number == count:
            elapsed = time.monotonic() - started
            print(f"{kind}: {number}/{count} files, {elapsed:.1f}s", flush=True)
    return {"kind": kind, "root": str(directory), "fileCount": count, "contentBytes": total_bytes, "perFileNominalBytes": per_file}


def generate(root: Path, force: bool) -> dict:
    root = root.resolve()
    usage = shutil.disk_usage(root.anchor)
    required = SOURCE_BYTES + LOG_BYTES + HUGE_BYTES + 4 * 1024**2
    if usage.free < required:
        raise RuntimeError(f"BLOCKED_INSUFFICIENT_DISK free={usage.free} required={required}")
    manifest_path = root / "corpus-manifest.json"
    if manifest_path.exists() and not force:
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
        if data.get("seed") == SEED and all(Path(v["root"]).exists() for v in data.get("corpora", [])):
            print(json.dumps(data, ensure_ascii=False, indent=2))
            return data
    root.mkdir(parents=True, exist_ok=True)
    corpora = [
        generate_kind(root, "SOURCE_CONFIG", SOURCE_FILES, SOURCE_BYTES, ".txt"),
        generate_kind(root, "LOG", LOG_FILES, LOG_BYTES, ".log"),
        generate_kind(root, "HUGE", HUGE_FILES, HUGE_BYTES, ".txt"),
    ]
    fixtures = root / "SOURCE_CONFIG" / "fixtures"
    fixtures.mkdir(parents=True, exist_ok=True)
    write_fixture(fixtures / "utf16le.txt", "CONTENT_UTF16\n障害復旧\n", "utf16le", True)
    write_fixture(fixtures / "utf16be.txt", "CONTENT_UTF16\n障害復旧\n", "utf16be", True)
    write_fixture(fixtures / "utf8bom.txt", "CONTENT_UTF16\n", "utf8", True)
    write_fixture(fixtures / "cp932.txt", "旧字障害\nCONTENT_CP932\n", "cp932", False)
    write_fixture(fixtures / "casefold.txt", "Straße STRASSE ẞ ss ﬃ ffi Σ σ ς K K\n", "utf8", False)
    (fixtures / "binary.txt").write_bytes(b"binary\x00\x00\x01\xff")
    all_files = [p for p in root.rglob("*") if p.is_file()]
    searchable = [p for p in all_files if p.suffix.lower() in SUPPORTED]
    manifest = {
        "version": 1,
        "seed": SEED,
        "generatedUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "root": str(root),
        "freeBytesAtStart": usage.free,
        "corpora": corpora,
        "fixtureFiles": len(all_files) - sum(x["fileCount"] for x in corpora),
        "actualFileCount": len(all_files),
        "actualSearchableFileCount": len(searchable),
        "actualSearchableBytes": sum(p.stat().st_size for p in searchable),
        "logicalSourceBytes": 100 * 1024**3,
    }
    canonical = json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    manifest["manifestSha256"] = hashlib.sha256(canonical).hexdigest()
    manifest_path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(manifest, ensure_ascii=False, indent=2))
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()
    try:
        generate(Path(args.root), args.force)
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
