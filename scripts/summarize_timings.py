"""Summarize synthetic benchmark CSVs. Phase spans nest: do not add their totals."""

import csv
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path

root = Path(sys.argv[1])
print("group,count,median_ms,p95_ms,min_ms,max_ms,total_ms")


def summarize(groups):
    for name, values in sorted(groups.items()):
        values.sort()
        p95 = values[math.ceil(len(values) * 0.95) - 1]
        print(f"{name},{len(values)},{statistics.median(values):.6f},"
              f"{p95:.6f},{min(values):.6f},{max(values):.6f},{sum(values):.6f}")


for filename in ["release.csv", "profiled.csv"]:
    groups = defaultdict(list)
    with (root / filename).open() as stream:
        for row in csv.DictReader(stream):
            groups[f"{filename}/{row['mode']}/{row['operation']}"].append(float(row["elapsed_ms"]))
    if not groups:
        raise SystemExit(f"No benchmark measurements in {filename}")
    summarize(groups)

groups = defaultdict(list)
for path in sorted((root / "phases").glob("profile-*.csv")):
    with path.open() as stream:
        for phase, pid, micros in csv.reader(stream):
            groups[f"phase/{phase}"].append(int(micros) / 1000)
if not groups:
    raise SystemExit("No phase measurements found")
summarize(groups)
