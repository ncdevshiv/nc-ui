"""Aligns the fuzz targets with the real `fuzz_target!` signature.

The targets were written as `fuzz_target!(name, |data: &[u8]| { .. })` to suit the stub in the
compile-check, which is backwards: the targets must compile against *libfuzzer-sys*, and the
stub is what adapts. So the targets go back to the real one-argument form and the stub derives
a per-module function name instead of a caller-supplied one.

Concretely: every target's `fuzz_target!(fuzz_name, |data: &[u8]| { .. });` becomes
`fuzz_target!(|data: &[u8]| { .. });`, and the compile-check addresses each body as
`<module>::fuzz_target_body`.
"""

import re
from pathlib import Path

for path in sorted(Path("fuzz/fuzz_targets").glob("*.rs")):
    source = path.read_text(encoding="utf-8")
    # Strip the leading name argument: `fuzz_target!(fuzz_thing, |data: ...` -> `fuzz_target!(|data: ...`
    updated, count = re.subn(r"fuzz_target!\(\s*fuzz_[a-z_]+\s*,\s*", "fuzz_target!(", source)
    if count:
        path.write_text(updated, encoding="utf-8", newline="")
        print(f"{path.name}: {count} call site(s) aligned")
    else:
        print(f"{path.name}: already aligned")