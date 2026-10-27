#!/usr/bin/env python3
"""The styling rule (CONTRIBUTING.md): views take colors from `cx.palette()` and sizes from
`cx.metrics()`. This fails on what bypasses them in the UI crates' code:

- `cx.theme()`: gpui-kit's theme, which only the kit mapping (`kit_theme.rs`) builds;
- a `px(…)` with a number in it that doesn't follow the zoom (`m.dp(8.)`, `z(8.)` or
  `px(n * m.zoom)` do); `px(0.)` and the 1 px hairline are fine;
- `text_xs()`/`text_sm()`/…: kit text sizes, which ignore the text size setting;
- `.truncate()`: cut text shows its whole text on hover through
  `gitweft_ui::truncated`; a container that only clips mixed content says so with a
  trailing `// untooltipped: …`.

A line that must keep a fixed size says why in a trailing `// unscaled: …` comment. Tests
are left out (a file's `#[cfg(test)] mod` and all after it: it sits at the end of the
file), and so are comment lines and `metrics.rs`, which defines the sizes.
    style-audit.py [ROOT…]    scan ROOT… (default: the workspace and ui crates' src)
    style-audit.py --self-test
"""
import re
import sys
import tempfile
from pathlib import Path

ROOTS = ["crates/gitweft-workspace/src", "crates/gitweft-ui/src"]
EXEMPT = {"metrics.rs", "kit_theme.rs", "truncated.rs"}
THEME = re.compile(r"\bcx\.theme\(\)")
KIT_TEXT = re.compile(r"\.text_(xs|sm|base|lg|xl|2xl|3xl)\(\)")
TRUNCATE = re.compile(r"\.truncate\(\)")
# The `px()` function, not the `.px()` padding method.
PX = re.compile(r"(?<![.\w])px\(")
HAIRLINE = re.compile(r"^\s*-?(0|0\.|0\.0|1\.|1\.0)\s*$")
# Scaled already: inside the call (`px(n * m.zoom)`) or just after it (`px(n) * m.zoom`).
ZOOMED = re.compile(r"^\s*\*\s*[\w.]*zoom\b")


def px_args(line):
    """The argument text of each `px(` call on `line`, and the text after the call."""
    for match in PX.finditer(line):
        depth, start = 1, match.end()
        for ix in range(start, len(line)):
            if line[ix] == "(":
                depth += 1
            elif line[ix] == ")":
                depth -= 1
                if depth == 0:
                    yield line[start:ix], line[ix + 1 :]
                    break


def scan_file(path):
    lines = path.read_text().splitlines()
    for number, line in enumerate(lines, 1):
        # The test module; a test-only function or `mod x;` elsewhere is scanned like the rest.
        if line.strip() == "#[cfg(test)]" and lines[number:number + 1] and re.match(
            r"\s*(pub(\(\w+\))? )?mod \w+\s*\{", lines[number]
        ):
            return
        code = line.split("//", 1)[0] if "//" in line and '"' not in line else line
        if line.lstrip().startswith("//") or "// unscaled:" in line or "// untooltipped:" in line:
            continue
        why = None
        if THEME.search(code):
            why = "cx.theme(): take colors from cx.palette(), sizes from cx.metrics()"
        elif KIT_TEXT.search(code):
            why = "a kit text size: use cx.metrics().text"
        elif TRUNCATE.search(code):
            why = ".truncate(): use gitweft_ui::truncated (a tooltip when cut), or `// untooltipped: why`"
        else:
            for arg, after in px_args(code):
                scaled = "zoom" in arg or ZOOMED.match(after)
                if re.search(r"\d", arg) and not scaled and not HAIRLINE.match(arg):
                    why = f"px({arg}) doesn't follow the zoom: use m.dp(…) or a Metrics size"
                    break
        if why:
            yield f"{path}:{number}: {why}\n    {line.strip()}"


def scan(roots):
    hits = []
    for root in roots:
        for path in sorted(Path(root).rglob("*.rs")):
            if path.name not in EXEMPT:
                hits.extend(scan_file(path))
    return hits


def self_test():
    with tempfile.TemporaryDirectory() as dir:
        Path(dir, "view.rs").write_text(
            "fn a() { div().gap(px(8.)) }\n"
            "fn b() { cx.theme().is_dark() }\n"
            "fn c() { div().text_xs() }\n"
            "fn d() { div().truncate().child(name) }\n"
            "fn cell() { div().w(w).truncate() } // untooltipped: a cell\n"
            "fn ok() { div().gap(m.dp(8.)).w(px(1.)).h(px(4. * m.zoom)).px(z(3.)) }\n"
            "fn ok2() { div().w(px(720.) * m.zoom).mb(px(-1.)) }\n"
            "fn kept() { ListState::new(0, px(200.)) } // unscaled: overdraw\n"
            "// px(8.) in a comment\n"
            "#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\nmod ui_test;\n#[cfg(test)]\nmod tests { fn t() { px(300.); } }\n"
        )
        hits = scan([dir])
    if len(hits) != 4 or not all(f"view.rs:{n}:" in h for n, h in zip((1, 2, 3, 4), hits)):
        print("style-audit: the self-test found", hits, "not lines 1-4", file=sys.stderr)
        return 1
    print("style-audit: the self-test's px literal, theme read, kit text size and truncate are caught")
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    hits = scan(sys.argv[1:] or ROOTS)
    for hit in hits:
        print(hit)
    if hits:
        print(f"style-audit: {len(hits)} lines break the styling rules (above)", file=sys.stderr)
        sys.exit(1)
