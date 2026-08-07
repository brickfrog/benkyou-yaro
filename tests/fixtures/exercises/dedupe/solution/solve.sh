#!/bin/sh
# Reference solution. Runs with the workspace as the working directory.
set -e
cat > solution.py <<'PY'
def dedupe(xs):
    seen = set()
    out = []
    for x in xs:
        if x not in seen:
            seen.add(x)
            out.append(x)
    return out
PY
