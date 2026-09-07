#!/usr/bin/env python3
"""Aggregate the isolated backend/corpus reports into the formal bakeoff set."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
from pathlib import Path

BACKENDS = ["scan", "bloom", "trigram", "sqlite-fts5"]
CORPORA = ["SOURCE_CONFIG", "LOG", "HUGE"]


def geo(values: list[float]) -> float:
    positives = [max(1e-9, float(v)) for v in values]
    return math.exp(sum(math.log(v) for v in positives) / len(positives)) if positives else 0.0


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8-sig"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report-root", required=True)
    parser.add_argument("--lock", required=True)
    parser.add_argument("--manifest-root", required=True)
    parser.add_argument("--output-summary", required=True)
    args = parser.parse_args()
    report_root = Path(args.report_root).resolve()
    lock = load(Path(args.lock))
    reports: list[dict] = []
    missing: list[str] = []
    for corpus in CORPORA:
        for backend in BACKENDS:
            path = report_root / f"{corpus}_{backend}.json"
            if not path.exists():
                missing.append(str(path))
            else:
                item = load(path)
                item["reportPath"] = str(path)
                reports.append(item)

    rows: list[dict] = []
    for backend in BACKENDS:
        selected = [r for r in reports if r.get("backend") == backend]
        correctness_ok = all(r.get("correctness", {}).get("fp") == 0 and r.get("correctness", {}).get("fn") == 0 for r in selected)
        runtime_ok = len(selected) == len(CORPORA) and all(r.get("exitCode", 1) == 0 for r in selected)
        p95s = []
        ready = []
        persistent_ratio = []
        builds = []
        cancel = []
        update_rebuilds = []
        for report in selected:
            p95s.append(report.get("overall", {}).get("fullP95Ms", 0))
            b = report.get("build", {})
            ready.append(b.get("readyPrivateBytes", 0))
            source_bytes = max(1, b.get("sourceBytes", 1))
            persistent_ratio.append(b.get("persistentBytes", 0) / source_bytes)
            builds.append(b.get("elapsedMs", 0))
            if report.get("cancellation"):
                cancel.append(report["cancellation"].get("P95Ms", 0))
            if report.get("updates"):
                update_rebuilds.append(report["updates"].get("FullBaseRewriteCount", 0))
        rows.append({
            "backend": backend,
            "eligible": correctness_ok and runtime_ok,
            "correctness": {"fp": sum(r.get("correctness", {}).get("fp", 0) for r in selected), "fn": sum(r.get("correctness", {}).get("fn", 0) for r in selected)},
            "searchFullP95GeomeanMs": geo(p95s),
            "readyPrivateMaxBytes": max(ready, default=0),
            "persistentRatioGeomean": geo(persistent_ratio),
            "initialBuildGeomeanMs": geo(builds),
            "cancelP95MaxMs": max(cancel, default=None),
            "fullBaseRewriteMax": max(update_rebuilds, default=0),
            "corporaMeasured": len(selected),
        })

    eligible = [r for r in rows if r["eligible"]]
    if eligible:
        best = {
            "searchFullP95GeomeanMs": min(r["searchFullP95GeomeanMs"] for r in eligible),
            "readyPrivateMaxBytes": min(r["readyPrivateMaxBytes"] for r in eligible),
            "persistentRatioGeomean": min(r["persistentRatioGeomean"] for r in eligible),
            "initialBuildGeomeanMs": min(r["initialBuildGeomeanMs"] for r in eligible),
        }
        for row in rows:
            if not row["eligible"]:
                row["score"] = None
                continue
            # Relative-best score; lower latency/memory/disk/build is better.
            parts = [
                best["searchFullP95GeomeanMs"] / max(1e-9, row["searchFullP95GeomeanMs"]),
                best["readyPrivateMaxBytes"] / max(1, row["readyPrivateMaxBytes"]),
                best["persistentRatioGeomean"] / max(1e-9, row["persistentRatioGeomean"]),
                1.0 if row["fullBaseRewriteMax"] == 0 else 0.0,
                best["initialBuildGeomeanMs"] / max(1e-9, row["initialBuildGeomeanMs"]),
                1.0,
                1.0 if row["correctness"]["fp"] == 0 and row["correctness"]["fn"] == 0 else 0.0,
            ]
            weights = [0.25, 0.20, 0.15, 0.15, 0.10, 0.10, 0.05]
            row["score"] = sum(a * b for a, b in zip(parts, weights))
        ranked = sorted((r for r in rows if r.get("score") is not None), key=lambda r: r["score"], reverse=True)
    else:
        ranked = []
    for index, row in enumerate(ranked):
        row["classification"] = "WINNER" if index == 0 else "SECONDARY" if index == 1 else "REJECTED"
    for row in rows:
        row.setdefault("classification", "REJECTED")

    for backend in BACKENDS:
        source = next((r for r in reports if r.get("backend") == backend and r.get("corpus") == "SOURCE_CONFIG"), None)
        update = {
            "version": 1,
            "seriesId": lock.get("seriesId"),
            "sourceCommitSha": lock.get("formal_source_commit_sha"),
            "backend": backend,
            "corpus": "SOURCE_CONFIG",
            "updates": source.get("updates") if source else None,
            "full_base_rewrite_count": source.get("updates", {}).get("FullBaseRewriteCount", 0) if source else None,
            "pass": bool(source and source.get("updates", {}).get("FullBaseRewriteCount", 0) == 0),
        }
        (report_root / f"UPDATE_{backend.upper().replace('-', '_')}.json").write_text(json.dumps(update, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    cancel = {
        "version": 1,
        "seriesId": lock.get("seriesId"),
        "sourceCommitSha": lock.get("formal_source_commit_sha"),
        "backends": {r["backend"]: r.get("cancellation") for r in reports if r.get("cancellation")},
        "threshold": {"p95Ms": 100},
    }
    (report_root / "CANCELLATION.json").write_text(json.dumps(cancel, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    summary = {
        "version": 1,
        "seriesId": lock.get("seriesId"),
        "formal_source_commit_sha": lock.get("formal_source_commit_sha"),
        "reportHeadSha": lock.get("report_head_sha", "pending"),
        "benchmark": lock.get("benchmark"),
        "reports": [r.get("reportPath") for r in reports],
        "missingReports": missing,
        "backendComparison": rows,
        "winner": ranked[0]["backend"] if ranked else None,
        "secondary": ranked[1]["backend"] if len(ranked) > 1 else None,
        "rejected": [r["backend"] for r in rows if r.get("classification") == "REJECTED"],
        "virtualPerformanceTargets": {
            "rareFullP95Ms": 50,
            "generalFullP95Ms": 100,
            "firstUsefulP95Ms": 100,
            "additionalReadyRamBytes": 536870912,
            "cancelP95Ms": 100,
        },
        "evaluationComplete": not missing and len(reports) == 12 and all(r.get("exitCode", 1) == 0 for r in reports),
        "pass": not missing and len(reports) == 12 and all(r.get("correctness", {}).get("fp") == 0 and r.get("correctness", {}).get("fn") == 0 for r in reports),
    }
    output = Path(args.output_summary).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    acceptance = {
        "version": 1,
        "evaluation_complete": summary["evaluationComplete"],
        "pass": summary["pass"],
        "seriesId": summary["seriesId"],
        "formal_source_commit_sha": summary["formal_source_commit_sha"],
        "winner": summary["winner"],
        "secondary": summary["secondary"],
        "rejected": summary["rejected"],
        "allowedNotRun": [],
        "performanceMissesAreResults": True,
    }
    (report_root / "CONTENT_SEARCH_BAKEOFF_ACCEPTANCE.json").write_text(json.dumps(acceptance, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")

    lines = [
        "# Content Search Bakeoff Formal Report",
        "",
        f"- Series: `{summary['seriesId']}`",
        f"- Measured source: `{summary['formal_source_commit_sha']}`",
        f"- Evaluation complete: `{summary['evaluationComplete']}`",
        f"- Correctness pass: `{summary['pass']}`",
        "",
        "| backend | class | eligible | p95 geomean ms | ready private max | persistent ratio | build geomean ms | score |",
        "|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        lines.append(f"| {row['backend']} | {row['classification']} | {row['eligible']} | {row['searchFullP95GeomeanMs']:.3f} | {row['readyPrivateMaxBytes']} | {row['persistentRatioGeomean']:.6f} | {row['initialBuildGeomeanMs']:.3f} | {row.get('score') if row.get('score') is not None else 'n/a'} |")
    lines += ["", "Performance target misses remain visible in each backend report and are not removed as outliers.", ""]
    (report_root.parent.parent / "docs" / "CONTENT_SEARCH_BAKEOFF_FINAL_REPORT.md").write_text("\n".join(lines), encoding="utf-8")
    print(json.dumps({"evaluationComplete": summary["evaluationComplete"], "pass": summary["pass"], "winner": summary["winner"]}, ensure_ascii=False))
    return 0 if summary["evaluationComplete"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
