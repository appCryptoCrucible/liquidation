# Daily digest: taxonomy from watcher SQLite + outcome JSONL. No invented numbers.

import argparse
import json
import sqlite3
import sys
from collections import Counter
from pathlib import Path


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--sqlite", required=True)
    p.add_argument("--jsonl", required=True)
    args = p.parse_args()
    sqlite = Path(args.sqlite)
    jsonl = Path(args.jsonl)
    if not sqlite.is_file():
        print(f"FAIL sqlite missing: {sqlite}", file=sys.stderr)
        return 1
    if not jsonl.is_file():
        print(f"FAIL jsonl missing: {jsonl}", file=sys.stderr)
        return 1
    con = sqlite3.connect(f"file:{sqlite.as_posix()}?mode=ro", uri=True)
    try:
        n = con.execute("SELECT COUNT(*) FROM liquidations").fetchone()[0]
    except sqlite3.Error as e:
        print(f"FAIL sqlite: {e}", file=sys.stderr)
        return 1
    names: Counter[str] = Counter()
    jsonl_rows = 0
    halts = 0
    with jsonl.open(encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            jsonl_rows += 1
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as e:
                print(f"FAIL jsonl: {e}", file=sys.stderr)
                return 1
            if "halt" in obj:
                halts += 1
            name = obj.get("outcome") or obj.get("name")
            if name:
                names[str(name)] += 1
    print(f"sqlite_rows={n}")
    print(f"jsonl_rows={jsonl_rows}")
    print(f"NotTracked={names.get('NotTracked', 0)}")
    print(f"HealthWrong={names.get('HealthWrong', 0)}")
    print(f"Declined={names.get('Declined', 0)}")
    print(f"halts={halts}")
    print("by_outcome=" + json.dumps(dict(names), sort_keys=True))
    if names.get("NotTracked", 0) or names.get("HealthWrong", 0):
        print("ALARM_COUNTS_NONZERO", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
