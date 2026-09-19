#!/usr/bin/env bash
# Layer watchdog: dependencies descend, data comes back up through return values.
#
# INTENT: the layering is the load-bearing decision of this repository. A prose
# rule nobody can run is a rule that decays, so it is checked mechanically.
#
#   L0  findery-core                            the vocabulary, depends on nothing
#   L1  findery-chunk findery-embed findery-store findery-walk   one concern each, core only
#   L1  findery-client                          the wire, core only
#   L2  findery-index                           the engine
#   L3  findery-server                          the HTTP transport
#   L4  findery-mcp findery-cli                      the edges
#
# Exit codes: 0 every edge descends, 1 at least one climbs.

set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

declare -A LAYER=(
  [findery-core]=0
  [findery-chunk]=1
  [findery-embed]=1
  [findery-store]=1
  [findery-walk]=1
  [findery-client]=1
  [findery-index]=2
  [findery-server]=3
  [findery-mcp]=4
  [findery-cli]=4
  [findery-bench]=9
)

violations=0
checked=0

for manifest in crates/*/Cargo.toml; do
  crate="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$manifest" | head -1)"
  [ -n "$crate" ] || continue
  own="${LAYER[$crate]:-}"
  if [ -z "$own" ]; then
    echo "R-00  $crate is not in the layer map; add it to $0"
    violations=$((violations + 1))
    continue
  fi

  # Internal dependencies only: `se-*` entries on the left-hand side.
  deps="$(sed -n 's/^\(findery-[a-z]*\) *=.*/\1/p' "$manifest" | sort -u)"
  for dep in $deps; do
    [ "$dep" = "$crate" ] && continue
    other="${LAYER[$dep]:-}"
    if [ -z "$other" ]; then
      echo "R-00  $crate depends on unknown crate $dep"
      violations=$((violations + 1))
      continue
    fi
    checked=$((checked + 1))
    if [ "$other" -ge "$own" ]; then
      echo "R-01  $crate (L$own) depends on $dep (L$other) — an edge that climbs"
      violations=$((violations + 1))
    fi
  done
done

# R-02: L0 must stay free of every internal dependency.
if grep -qE '^findery-' crates/core/Cargo.toml; then
  echo "R-02  findery-core depends on another workspace crate; L0 depends on nothing"
  violations=$((violations + 1))
fi

# R-03: the client must stay light enough to drop into any project.
for heavy in findery-store findery-index findery-chunk findery-embed; do
  if grep -qE "^$heavy *=" crates/client/Cargo.toml; then
    echo "R-03  findery-client depends on $heavy; the client carries the wire, not the daemon"
    violations=$((violations + 1))
  fi
done

# R-04: the MCP bridge talks HTTP, never to the engine directly.
if grep -qE '^findery-(index|store|chunk|embed) *=' crates/mcp/Cargo.toml; then
  echo "R-04  findery-mcp reaches past the HTTP API into the engine"
  violations=$((violations + 1))
fi

if [ "$violations" -eq 0 ]; then
  echo "layers OK — $checked internal edges, all descending"
  exit 0
fi

echo
echo "$violations violation(s). Dependencies descend; data comes back through Result."
exit 1
