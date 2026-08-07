#!/bin/sh
# The grader. Exit code reports grader health, never the grade; the grade goes in
# out/reward.json. Runs with the run directory as the working directory.
mkdir -p out

if ! command -v python3 >/dev/null 2>&1; then
  # The grader itself cannot run. Say so by exiting non-zero and writing nothing.
  echo "python3 not available" >&2
  exit 70
fi

output=$(python3 check/cases.py work/solution.py 2>&1)
status=$?

if [ "$status" -eq 0 ]; then
  score=1
else
  score=0
fi

# Escape the detail for JSON: drop quotes and backslashes, collapse newlines.
detail=$(printf '%s' "$output" | tr '\n' ' ' | tr -d '"\\' | cut -c1-300)

printf '{"correctness": %s, "detail": "%s"}\n' "$score" "$detail" > out/reward.json
exit 0
