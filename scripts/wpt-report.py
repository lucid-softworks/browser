#!/usr/bin/env python3
"""Build a multi-page, coverage-style HTML report of WPT results.

Usage: scripts/wpt-report.py <source> [out-dir] [area-label]
  <source>   a per-test result store directory (the usual case, written by wpt-ingest.py) OR a
             single wptreport JSON file.
  out-dir    container directory (default: wpt-report). Pages are written under <out-dir>/site/ and
             the entry point is <out-dir>/index.html (a redirect into site/).
  area-label the top-level area to root at (default: css). Pass `all` to render every area in the
             store under a synthetic `wpt` root (the whole-suite report).

Layout (like an lcov coverage report): one page per directory. Every directory of the test tree gets
its own `index.html` listing its immediate children — subdirectories (aggregate pass/total + a bar,
click to drill in) and test files (status badge, pass/fail subtest counts, first failure).
Breadcrumbs link back up. A file is "fully passing" when its file-level status is OK/PASS and it has
no failing subtest; "broken" otherwise.
"""
import html
import json
import os
import shutil
import sys
from collections import Counter

GOOD_FILE = {"OK", "PASS"}
GOOD_SUB = {"PASS"}


def first_failure(r):
    if r["status"] not in GOOD_FILE and r.get("message"):
        return f"{r['status']}: {r['message']}"
    for s in r.get("subtests", []):
        if s["status"] not in GOOD_SUB:
            name = s.get("name", "")
            msg = s.get("message") or s["status"]
            return f"{name}: {msg}" if name else msg
    return r["status"]


def load_results(source):
    """Load results from a wptreport JSON file OR a store directory of per-test `*.json` files
    (each holding one result object). Mirrors the per-test store the report tooling maintains so a
    single re-run only rewrites one small file."""
    if os.path.isdir(source):
        out = []
        for dirpath, _dirs, names in os.walk(source):
            for n in names:
                if n.endswith(".json"):
                    try:
                        out.append(json.load(open(os.path.join(dirpath, n))))
                    except (ValueError, OSError):
                        pass
        return out
    return json.load(open(source)).get("results", [])


class Node:
    """A directory in the test tree."""

    __slots__ = ("name", "dirs", "files", "agg")

    def __init__(self, name):
        self.name = name
        self.dirs = {}   # child dir name -> Node
        self.files = []  # (filename, result_dict, pass_subs, fail_subs, broken)
        self.agg = (0, 0, 0, 0)


def build_tree(results, area):
    """Build the directory tree. `area` is the single top-level area to root at (its leading path
    segment is stripped). Pass None to render EVERY area under a synthetic root, keeping each test's
    full path — used for the whole-suite report spanning css/, dom/, html/, … ."""
    root = Node(area if area else "wpt")
    for r in results:
        parts = r["test"].lstrip("/").split("/")
        if area:
            if not parts or parts[0] != area:
                # Tolerate results outside the area by bucketing them under the root.
                parts = [area] + parts
            path = parts[1:]  # drop the area segment (it's the root)
        else:
            path = parts  # synthetic root: top-level areas become its child dirs
        node = root
        for seg in path[:-1]:
            node = node.dirs.setdefault(seg, Node(seg))
        fname = path[-1] if path else r["test"]
        subs = r.get("subtests", [])
        sp = sum(1 for s in subs if s["status"] in GOOD_SUB)
        sf = len(subs) - sp
        broken = r["status"] not in GOOD_FILE or sf > 0
        node.files.append((fname, r, sp, sf, broken))
    return root


def aggregate(node):
    """Recursively compute (total_files, pass_files, pass_subs, total_subs) for a dir node."""
    tot = passf = ps = ts = 0
    for _, _r, sp, sf, broken in node.files:
        tot += 1
        passf += 0 if broken else 1
        ps += sp
        ts += sp + sf
    for child in node.dirs.values():
        ct, cp, cps, cts = aggregate(child)
        tot += ct
        passf += cp
        ps += cps
        ts += cts
    node.agg = (tot, passf, ps, ts)
    return node.agg


STYLE = """
:root{color-scheme:light dark}
*{box-sizing:border-box}
body{font-family:system-ui,sans-serif;margin:0;background:#fff;color:#1a1a1a}
header{padding:22px 32px 18px;border-bottom:1px solid #ddd}
.crumbs{font-size:13px;color:#666;margin-bottom:10px;font-family:ui-monospace,monospace}
.crumbs a{color:#06c;text-decoration:none}.crumbs a:hover{text-decoration:underline}
h1{margin:0 0 10px;font-size:20px}
h1 code{font-size:18px}
.score{font-size:40px;font-weight:700;margin:6px 0 4px}
.score small{font-size:18px;color:#666;font-weight:400}
.track{height:9px;border-radius:5px;background:#eee;overflow:hidden;max-width:520px}
.fill{height:100%}
.meta{margin-top:9px;color:#555;font-size:13px}
table{border-collapse:collapse;width:100%;font-size:13px}
td,th{padding:6px 12px;border-bottom:1px solid #eee;text-align:left;vertical-align:top}
th{position:sticky;top:0;background:#fafafa;font-size:12px;color:#666;cursor:default}
.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.num.bad{color:#d33;font-weight:600}
.name{font-family:ui-monospace,monospace;color:#222;word-break:break-all}
.name a{color:#06c;text-decoration:none}.name a:hover{text-decoration:underline}
.dir .name a{font-weight:600}
.det{color:#b00;font-size:12px;margin-top:3px;font-family:ui-monospace,monospace;word-break:break-word}
.b{display:inline-block;min-width:64px;text-align:center;padding:2px 8px;border-radius:4px;font-size:11px;font-weight:700;color:#fff}
.b.pass{background:#2a2}.b.fail{background:#d33}.b.timeout{background:#999}.b.error{background:#a0a}.b.dir{background:#06c}
.minibar{display:inline-block;vertical-align:middle;width:90px;height:8px;border-radius:4px;background:#eee;overflow:hidden;margin-right:8px}
.minibar>i{display:block;height:100%}
.pct{font-variant-numeric:tabular-nums}
@media (prefers-color-scheme: dark){
  body{background:#15171a;color:#e6e6e6}header{border-bottom-color:#2a2d31}
  .sub,.meta,.score small,.crumbs{color:#9aa0a6}.track,.minibar{background:#2a2d31}
  td,th{border-bottom-color:#23262a}th{background:#1b1e22;color:#9aa0a6}
  .name{color:#cfd3d7}.name a,.crumbs a{color:#5aa3f0}.det{color:#ff8a8a}.num.bad{color:#ff6b6b}
}
"""


def bar_color(pct):
    # red -> amber -> green
    if pct >= 90:
        return "#2a2"
    if pct >= 50:
        return "#e90"
    return "#d33"


def pct_of(passf, tot):
    return (passf / tot * 100) if tot else 100.0


def status_badge(r):
    st = r["status"]
    cls = {"OK": "fail", "FAIL": "fail", "TIMEOUT": "timeout",
           "ERROR": "error", "CRASH": "error", "PASS": "pass"}.get(st, "fail")
    # An OK file with no failing subtests is a pass; OK with failures shows FAIL.
    label = st
    if st == "OK":
        label = "FAIL"  # only broken files reach the table cell that calls this for OK
    if st == "PASS":
        cls = "pass"
    return cls, label


def page_html(node, rel_to_root, area, depth):
    tot, passf, ps, ts = node.agg
    pct = pct_of(passf, tot)
    # Breadcrumbs: area / seg / seg (each links to its index, last is plain).
    # rel_to_root is the list of path segments from the area root to this node.
    crumbs = []
    up = len(rel_to_root)
    # link to root
    root_href = ("../" * up) + "index.html" if up else "index.html"
    crumbs.append(f'<a href="{root_href}">{html.escape(area)}</a>')
    for i, seg in enumerate(rel_to_root):
        ups = up - i - 1
        if ups == 0:
            crumbs.append(html.escape(seg))
        else:
            href = ("../" * ups) + "index.html"
            crumbs.append(f'<a href="{href}">{html.escape(seg)}</a>')
    crumb_html = " / ".join(crumbs)

    rows = []
    # Subdirectories first, worst pass-rate first (surface problems), then name.
    subdirs = sorted(
        node.dirs.values(),
        key=lambda c: (pct_of(c.agg[1], c.agg[0]), c.name),
    )
    for c in subdirs:
        ct, cp, _cps, _cts = c.agg
        cpct = pct_of(cp, ct)
        col = bar_color(cpct)
        broken = ct - cp
        rows.append(
            f"<tr class=dir><td class=name><a href='{html.escape(c.name)}/index.html'>{html.escape(c.name)}/</a></td>"
            f"<td><span class='b dir'>DIR</span></td>"
            f"<td class=num><span class=minibar><i style='width:{cpct:.0f}%;background:{col}'></i></span>"
            f"<span class=pct>{cpct:.0f}%</span></td>"
            f"<td class=num>{cp}/{ct}</td>"
            f"<td class='num{' bad' if broken else ''}'>{broken}</td></tr>"
        )
    # Files: worst status first, then failing-subtest count desc, then name.
    order = {"CRASH": 0, "ERROR": 1, "TIMEOUT": 2, "FAIL": 3, "OK": 4, "PASS": 5}
    files = sorted(node.files, key=lambda f: (order.get(f[1]["status"], 9), -f[3], f[0]))
    for fname, r, sp, sf, broken in files:
        if broken:
            cls, label = status_badge(r)
            det = html.escape(first_failure(r))
            if len(det) > 300:
                det = det[:300] + "…"
            detail = f"<div class=det>{det}</div>"
            failcell = f"<td class='num bad'>{sf}</td>"
        else:
            cls, label = "pass", "PASS"
            detail = ""
            failcell = f"<td class=num>{sf}</td>"
        # Per-file subtest pass-rate bar (matching the directory rows); reftests with no subtests
        # (0/0) leave the cell empty.
        ftot = sp + sf
        if ftot > 0:
            fpct = pct_of(sp, ftot)
            ratecell = (
                f"<td class=num><span class=minibar>"
                f"<i style='width:{fpct:.0f}%;background:{bar_color(fpct)}'></i></span>"
                f"<span class=pct>{fpct:.0f}%</span></td>"
            )
        else:
            ratecell = "<td class=num></td>"
        rows.append(
            f"<tr class=file><td class=name>{html.escape(fname)}{detail}</td>"
            f"<td><span class='b {cls}'>{label}</span></td>"
            f"{ratecell}"
            f"<td class=num>{sp}</td>{failcell}</tr>"
        )

    title = area if not rel_to_root else "/".join([area] + rel_to_root)
    return f"""<!doctype html><html><head><meta charset=utf-8><title>WPT — {html.escape(title)}</title><style>{STYLE}</style></head><body>
<header>
<div class=crumbs>{crumb_html}</div>
<h1>Web Platform Tests — <code>{html.escape(title)}</code></h1>
<div class=score>{pct:.1f}% <small>{passf} / {tot} files fully pass</small></div>
<div class=track><div class=fill style="width:{pct:.1f}%;background:{bar_color(pct)}"></div></div>
<div class=meta>{tot - passf} broken · {tot} files · {ps} / {ts} subtests pass · {len(node.dirs)} subdirs</div>
</header>
<table><thead><tr><th>Name</th><th>Status</th><th class=num>Pass&nbsp;rate</th><th class=num>Pass</th><th class=num>Fail</th></tr></thead>
<tbody>
{chr(10).join(rows)}
</tbody></table>
</body></html>"""


def write_pages(node, base_dir, rel_to_root, area):
    out_dir = os.path.join(base_dir, *rel_to_root)
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "index.html"), "w") as f:
        f.write(page_html(node, rel_to_root, area, len(rel_to_root)))
    n = 1
    for name, child in node.dirs.items():
        n += write_pages(child, base_dir, rel_to_root + [name], area)
    return n


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    src = sys.argv[1]                                        # per-test store dir OR wptreport JSON
    out_dir = sys.argv[2] if len(sys.argv) > 2 else "wpt-report"
    area = sys.argv[3] if len(sys.argv) > 3 else "css"

    # `all` (or `*`) renders every top-level area under a synthetic `wpt` root, rather than rooting
    # at a single area. Used by the whole-suite report.
    multi = area in ("all", "*")
    if multi:
        area = "wpt"

    results = load_results(src)
    root = build_tree(results, None if multi else area)
    aggregate(root)

    # Everything lives under one container dir: pages under `<out_dir>/site/`, a tiny redirect at
    # `<out_dir>/index.html` (the entry point). The per-test store lives under the same container
    # (`<out_dir>/results/`) but is owned by the ingester, not touched here.
    site = os.path.join(out_dir, "site")
    if os.path.isdir(site):  # fresh page tree so deleted/renamed tests don't leave stale pages
        shutil.rmtree(site)
    pages = write_pages(root, site, [], area)

    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "index.html"), "w") as f:
        f.write(
            f"<!doctype html><meta charset=utf-8>"
            f"<title>WPT — {html.escape(area)}</title>"
            f"<meta http-equiv=refresh content=\"0; url=site/index.html\">"
            f"<body><a href=\"site/index.html\">WPT {html.escape(area)} report</a></body>"
        )

    tot, passf, ps, ts = root.agg
    print(f"wrote {out_dir}/index.html + {pages} pages — "
          f"{passf}/{tot} files pass ({pct_of(passf, tot):.1f}%), {tot - passf} broken")


if __name__ == "__main__":
    main()
