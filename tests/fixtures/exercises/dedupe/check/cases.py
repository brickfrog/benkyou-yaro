"""Hidden cases. Never copied into the learner's workspace."""
import sys
import importlib.util

spec = importlib.util.spec_from_file_location("solution", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

CASES = [
    ([3, 1, 3, 2, 1], [3, 1, 2]),
    ([], []),
    ([1], [1]),
    ([2, 2, 2], [2]),
    (["b", "a", "b"], ["b", "a"]),
    (list(range(50)) * 2, list(range(50))),
]

failures = []
for xs, want in CASES:
    original = list(xs)
    try:
        got = mod.dedupe(xs)
    except Exception as e:
        failures.append(f"dedupe({original!r}) raised {type(e).__name__}")
        continue
    if got != want:
        failures.append(f"dedupe({original!r}) == {got!r}, want {want!r}")
    if xs != original:
        failures.append(f"dedupe mutated its input: {xs!r}")

if failures:
    print("; ".join(failures[:3]))
    sys.exit(1)
print("all cases passed")
