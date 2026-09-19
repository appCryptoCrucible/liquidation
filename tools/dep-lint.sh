#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

while read -r from to; do
  [[ -z "${from:-}" || "${from:0:1}" == "#" ]] && continue
  if cargo tree -p "$from" -e normal --prefix none | awk '{print $1}' | grep -qx "$to"; then
    echo "forbidden dependency: $from -> $to"
    exit 1
  fi
done < forbid.txt

if cargo tree -p liq-types -e normal --prefix none | awk 'NR > 1 {print $1}' | grep -q '^liq-'; then
  echo "liq-types must not depend on any workspace crate"
  exit 1
fi

for c in liq-types liq-protocol liq-state liq-engine liq-flash liq-plan; do
  if cargo tree -p "$c" -e normal --prefix none | awk '{print $1}' | grep -qx tokio; then
    echo "$c must not depend on tokio"
    exit 1
  fi
done

echo "dep-lint: ok"
