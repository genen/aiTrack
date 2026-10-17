#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate THIRD-PARTY-NOTICES.md from `cargo metadata` (run from the repo root: python3 scripts/gen-third-party.py).

Lists every crate the build can pull in on any target (Windows, macOS, Linux), with its version, license
expression and source. A developer tool only; it is not part of the product.
"""
import json
import subprocess
import sys

meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"]))
rows = sorted(
    (p["name"], p["version"], p.get("license") or "see crate", p.get("repository") or "")
    for p in meta["packages"]
    if p["name"] != "aitrack"
)
out = []
out.append("# Third-party notices\n")
out.append(
    "aiTrack is free software under the **GNU General Public License v3.0 or later** (see [LICENSE](LICENSE)). "
    "It is built from the open-source Rust crates listed below. Each crate stays under its own license; the full "
    "license text of a crate is in its source package (https://crates.io/crates/NAME/VERSION) and in the "
    "repository linked on the same row.\n"
)
out.append(
    "Where a crate offers several licenses (`A OR B`), aiTrack uses it under the permissive one that is "
    "compatible with the GPL (for example MIT or Apache-2.0). No crate below is licensed GPL-incompatibly.\n"
)
out.append(
    "The `egui` crates bundle some fonts (`epaint_default_fonts`). Those fonts are under the licenses named in that "
    "crate's row (the SIL Open Font License 1.1 and the Ubuntu Font License 1.0, in addition to MIT or Apache-2.0 "
    "for the crate's own code); their texts are in that crate's source package.\n"
)
out.append(f"{len(rows)} crates. Generated with `python3 scripts/gen-third-party.py` from `Cargo.lock`.\n")
out.append("| Crate | Version | License | Source |")
out.append("|---|---|---|---|")
for n, v, lic, repo in rows:
    out.append(f"| {n} | {v} | {lic} | {repo} |")
sys.stdout.write("\n".join(out) + "\n")
