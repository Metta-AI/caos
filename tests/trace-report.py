#!/usr/bin/env python3
"""Summarize `caos-trace` lines from caos-server stderr.

Usage: trace-report.py <logfile> [--waterfall]

Each /run emits one line:
  caos-trace req=<hash> parent=<hash|-> depth=N outcome=<backend|hit>
    cache_ms=A resolve_ms=B dispatch_ms=C total_ms=T t=<unix-ms-at-emit>

The report gives totals, per-outcome counts, phase breakdown, and latency
percentiles; --waterfall additionally prints every run as an indented tree
with its offset from the first run's start.
"""

import re
import sys
from collections import defaultdict

LINE = re.compile(r"caos-trace (.*)")


def parse(path):
    runs = []
    with open(path, errors="replace") as f:
        for line in f:
            m = LINE.search(line)
            if not m:
                continue
            fields = dict(kv.split("=", 1) for kv in m.group(1).split())
            runs.append(
                {
                    "req": fields["req"],
                    "parent": fields["parent"],
                    "depth": int(fields["depth"]),
                    "outcome": fields["outcome"],
                    "cache_ms": int(fields["cache_ms"]),
                    "resolve_ms": int(fields["resolve_ms"]),
                    "dispatch_ms": int(fields["dispatch_ms"]),
                    "total_ms": int(fields["total_ms"]),
                    "end": int(fields["t"]),
                }
            )
    for r in runs:
        r["start"] = r["end"] - r["total_ms"]
    return runs


def pct(xs, p):
    if not xs:
        return 0
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p / 100))]


def summarize(runs):
    if not runs:
        print("no caos-trace lines found")
        return
    t0 = min(r["start"] for r in runs)
    t1 = max(r["end"] for r in runs)
    print(f"runs: {len(runs)}   wall-clock: {(t1 - t0) / 1000:.2f}s")
    by_outcome = defaultdict(list)
    for r in runs:
        by_outcome[r["outcome"]].append(r)
    for outcome, rs in sorted(by_outcome.items()):
        tot = [r["total_ms"] for r in rs]
        print(
            f"  {outcome:8} n={len(rs):5}  total p50={pct(tot, 50)}ms "
            f"p95={pct(tot, 95)}ms max={max(tot)}ms sum={sum(tot) / 1000:.1f}s"
        )
        for phase in ("cache_ms", "resolve_ms", "dispatch_ms"):
            xs = [r[phase] for r in rs]
            print(
                f"    {phase:12} p50={pct(xs, 50)}ms p95={pct(xs, 95)}ms "
                f"sum={sum(xs) / 1000:.1f}s"
            )
    depths = [r["depth"] for r in runs]
    print(f"  max depth: {max(depths)}")


def waterfall(runs):
    t0 = min(r["start"] for r in runs)
    children = defaultdict(list)
    for r in runs:
        children[r["parent"]].append(r)
    for rs in children.values():
        rs.sort(key=lambda r: r["start"])

    def emit(r, indent):
        off = r["start"] - t0
        print(
            f"{off:7}ms {'  ' * indent}{r['req'][:12]} {r['outcome']:6} "
            f"{r['total_ms']}ms (resolve {r['resolve_ms']} + dispatch {r['dispatch_ms']})"
        )
        for c in children.get(r["req"], []):
            emit(c, indent + 1)

    for root in children.get("-", []):
        emit(root, 0)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        sys.exit(__doc__)
    runs = parse(args[0])
    summarize(runs)
    if "--waterfall" in sys.argv:
        print()
        waterfall(runs)


if __name__ == "__main__":
    main()
