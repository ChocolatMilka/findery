#!/usr/bin/env bash
# Import watchdog: every block of `use` statements is grouped by layer (CONV-039).
#
#   1. std, core, alloc
#   2. external crates
#   3. findery_core                                    L0
#   4. findery_chunk, _embed, _store, _walk, _client   L1
#   5. findery_index                                   L2
#   6. findery_server                                  L3
#   7. crate, super, self, and the modules this file declares
#
# One blank line between two groups, none inside one; rustfmt sorts within a
# group and never moves a line across a blank one, so the two never fight.
#
# INTENT: the order of a file's imports is the layer law read from the top of
# the file. A reader sees at once what a module leans on, and an import from a
# layer above stands out instead of hiding in an alphabetical list.
#
# `pub use` re-exports are a module's surface, not its dependencies, and are
# left where they are. Exit codes: 0 every block is in order, 1 at least one
# is not.

set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

violations=0
checked=0

while IFS= read -r file; do
  checked=$((checked + 1))
  out=$(awk '
    function rank(root) {
      if (root == "std" || root == "core" || root == "alloc") return 1
      if (root == "crate" || root == "super" || root == "self" || (root in local)) return 7
      if (root == "findery_core") return 3
      if (root ~ /^findery_(chunk|embed|store|walk|client)$/) return 4
      if (root == "findery_index") return 5
      if (root == "findery_server") return 6
      return 2
    }
    FNR == NR {
      if (match($0, /^[ \t]*(pub(\([^)]*\))?[ \t]+)?mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*;/)) {
        line = $0
        sub(/^[ \t]*(pub(\([^)]*\))?[ \t]+)?mod[ \t]+/, "", line)
        sub(/[ \t]*;.*/, "", line)
        local[line] = 1
      }
      next
    }
    in_stmt { if ($0 ~ /;[ \t]*(\/\/.*)?$/) in_stmt = 0; next }
    /^[ \t]*use[ \t]/ {
      indent = $0; sub(/[^ \t].*/, "", indent)
      if (!region || indent != region_indent) { region = 1; region_indent = indent; prev = 0; blank = 0 }
      root = $0
      sub(/^[ \t]*use[ \t]+(::)?/, "", root)
      sub(/[^A-Za-z0-9_].*/, "", root)
      r = rank(root)
      if (prev) {
        if (r < prev)                 printf "%s:%d: `%s` belongs above the group before it\n", FILENAME, FNR, root
        else if (r > prev && !blank)  printf "%s:%d: `%s` starts a new group: a blank line goes above it\n", FILENAME, FNR, root
        else if (r == prev && blank)  printf "%s:%d: `%s` is in the same group as the line above: no blank line between\n", FILENAME, FNR, root
      }
      prev = r; blank = 0
      if ($0 !~ /;[ \t]*(\/\/.*)?$/) in_stmt = 1
      next
    }
    /^[ \t]*$/ { if (region) blank = 1; next }
    /^[ \t]*\/\/[^\/]/ { next }
    { region = 0 }
  ' "$file" "$file")
  if [ -n "$out" ]; then
    echo "$out" | sed 's/^/CONV-039  /'
    violations=$((violations + $(echo "$out" | wc -l)))
  fi
done < <(git ls-files 'crates/*.rs')

if [ "$violations" -gt 0 ]; then
  echo
  echo "$violations import(s) out of their layer group. Order: std, external, findery_core, L1, L2, L3, crate."
  exit 1
fi
echo "imports OK — $checked files, every block grouped by layer"
