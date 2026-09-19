#!/usr/bin/env bash
# File-size watchdog: no source file grows past its budget (CONV-029).
#
# INTENT: the rule exists so that one file holds one responsibility and stays
# readable. Test modules do not work against that — they sit next to the code
# they pin down, which is a Rust strength, not a smell. So the budget applies to
# what a reader has to hold in their head: the lines OUTSIDE `#[cfg(test)]`.
#
# Exit codes: 0 every file is within budget, 1 at least one is not.

set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

BUDGET="${1:-450}"
violations=0
checked=0

while IFS= read -r file; do
  checked=$((checked + 1))
  code=$(awk '
    /^[[:space:]]*#\[cfg\(test\)\]/ { intest = 1; next }
    intest {
      n = gsub(/{/, "{"); m = gsub(/}/, "}")
      depth += n - m
      if (depth <= 0) { intest = 0; depth = 0 }
      next
    }
    { count++ }
    END { print count + 0 }
  ' "$file")
  if [ "$code" -gt "$BUDGET" ]; then
    total=$(wc -l < "$file" | tr -d ' ')
    echo "CONV-029  $file: $code lines of code (budget $BUDGET, $total with tests)"
    violations=$((violations + 1))
  fi
done < <(find crates -name '*.rs' -type f -not -path '*/tests/*')

if [ "$violations" -eq 0 ]; then
  echo "file sizes OK — $checked files, all within $BUDGET lines of code"
  exit 0
fi

echo
echo "$violations file(s) over budget. Split by responsibility, not by line count."
exit 1
