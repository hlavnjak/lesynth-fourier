#!/usr/bin/env python3
# Copyright 2026 Jakub Hlavnicka
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""Write THIRD-PARTY-LICENSES.txt for one release build.

    third-party-licenses.py OUT TARGET-TRIPLE CRATE_DIR...

Every crate a release binary links — the normal (not dev, not build)
dependencies of each CRATE_DIR, resolved for TARGET-TRIPLE — with its licence
expression, authors, source and the licence files it ships. Many crates ship
none, and an MIT notice still has to travel with the binary, so the standard
text of every licence named is appended from packaging/release/licenses/.
The text above the list is packaging/release/THIRD-PARTY-HEADER.txt.
"""

import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RELEASE = os.path.join(HERE, "..", "packaging", "release")
OWN = {"resynth-studio", "lesynth_fourier"}

# Licence identifiers as crates write them -> file in packaging/release/licenses/.
TEXTS = {
    "MIT": "MIT", "Apache-2.0": "Apache-2.0", "ISC": "ISC", "MPL-2.0": "MPL-2.0",
    "GPLv3": "GPL-3.0", "GPL-3.0": "GPL-3.0", "BSL-1.0": "BSL-1.0", "CC0-1.0": "CC0-1.0",
    "OFL-1.1": "OFL-1.1", "Ubuntu-font-1.0": "Ubuntu-font-1.0", "Zlib": "Zlib",
    "Unicode-3.0": "Unicode-3.0", "Unicode-DFS-2016": "Unicode-DFS-2016",
    "BSD-3-Clause": "BSD-3-Clause", "BSD-2-Clause": "BSD-3-Clause",
}


def linked_packages(crate_dir, triple):
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked",
         "--filter-platform", triple], cwd=crate_dir))
    by_id = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen, stack = set(), [meta["resolve"]["root"]]
    while stack:
        pkg = stack.pop()
        if pkg in seen:
            continue
        seen.add(pkg)
        for dep in nodes[pkg]["deps"]:
            if any(k["kind"] in (None, "normal") for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    return [by_id[p] for p in seen if by_id[p]["name"] not in OWN]


def licence_files(pkg):
    root = os.path.dirname(pkg["manifest_path"])
    files = [os.path.join(root, f) for f in sorted(os.listdir(root))
             if re.match(r"(licen[cs]e|copying|notice|copyright)", f, re.I)
             and os.path.isfile(os.path.join(root, f))]
    if pkg.get("license_file"):
        extra = os.path.normpath(os.path.join(root, pkg["license_file"]))
        if os.path.isfile(extra) and extra not in files:
            files.append(extra)
    return files


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    out, triple, crate_dirs = sys.argv[1], sys.argv[2], sys.argv[3:]
    pkgs = {}
    for d in crate_dirs:
        for p in linked_packages(d, triple):
            pkgs[(p["name"], p["version"])] = p

    named = set()
    with open(out, "w", encoding="utf-8") as f:
        with open(os.path.join(RELEASE, "THIRD-PARTY-HEADER.txt"), encoding="utf-8") as h:
            f.write(h.read())
        for key in sorted(pkgs):
            p = pkgs[key]
            expr = p.get("license") or "see licence file"
            named |= {t for t in re.findall(r"[A-Za-z0-9.\-]+", expr) if t in TEXTS}
            f.write("-" * 72 + "\n")
            f.write(f"{p['name']} {p['version']}\nLicence: {expr}\n")
            if p.get("authors"):
                f.write("Authors: " + ", ".join(p["authors"]) + "\n")
            source = p.get("repository") or p.get("source") or ""
            if source:
                f.write(f"Source: {source}\n")
            files = licence_files(p)
            if not files:
                f.write("(No licence file ships with this crate; the standard text "
                        "of its licence is in the appendix.)\n")
            for path in files:
                f.write(f"\n--- {os.path.basename(path)} ---\n")
                with open(path, encoding="utf-8", errors="replace") as lf:
                    f.write(lf.read().rstrip() + "\n")
            f.write("\n")
        f.write("=" * 72 + "\nAPPENDIX: standard licence texts referred to above\n"
                + "=" * 72 + "\n")
        for name in sorted({TEXTS[t] for t in named}):
            f.write(f"\n{'#' * 72}\n# {name}\n{'#' * 72}\n\n")
            with open(os.path.join(RELEASE, "licenses", name), encoding="utf-8") as lf:
                f.write(lf.read().rstrip() + "\n")
    print(f"{out}: {len(pkgs)} crates")


if __name__ == "__main__":
    main()
