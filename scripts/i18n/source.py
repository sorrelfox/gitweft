#!/usr/bin/env python3
"""The English source for translators: locales/app.yml as JSON, in file order.

Usage: scripts/i18n/source.py [OUT]   (default target/i18n/en.json)
Writes {"keys": [[key, text], ...], "plural_bases": [...]}; a plural base is a key read
with tn! whose forms are `<base>_one`, `<base>_other` (and `_few`/`_many` elsewhere).
"""
import json
import os
import sys

import yaml

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
FORMS = ("_one", "_few", "_many", "_other")


def english():
    with open(os.path.join(ROOT, "locales", "app.yml"), encoding="utf-8") as f:
        data = yaml.safe_load(f)
    keys = []
    for key, value in data.items():
        if key.startswith("_"):
            continue
        keys.append((key, value["en"]))
    return keys


def plural_bases(keys):
    bases = []
    for key, _ in keys:
        for form in FORMS:
            if key.endswith(form):
                base = key[: -len(form)]
                if base not in bases:
                    bases.append(base)
    return bases


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "i18n", "en.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    keys = english()
    with open(out, "w", encoding="utf-8") as f:
        json.dump({"keys": keys, "plural_bases": plural_bases(keys)}, f, ensure_ascii=False, indent=1)
    print(f"{len(keys)} keys -> {out}")


if __name__ == "__main__":
    main()
