#!/usr/bin/env python3
"""Build an HTML report of broken WPT files from a wptreport JSON log.

Usage: scripts/wpt-report.py <wptreport.json> [out.html] [area-label]

A file is "broken" when its file-level status is not OK/PASS, or it ran fine
(OK) but has at least one non-PASS subtest. The report lists every broken file
with a status badge, pass/fail subtest counts, and the first failure message.
"""
import html
import json
import sys
from collections import Counter

GOOD_FILE = {"OK", "PASS"}
GOOD_SUB = {"PASS"}


def load(path):
    with open(path) as f:
        return json.load(f)


def first_failure(r):
    """A short detail string explaining why a file is broken."""
    if r["status"] not in GOOD_FILE and r.get("message"):
        return f"{r['status']}: {r['message']}"
    for s in r.get("subtests", []):
        if s["status"] not in GOOD_SUB:
            name = s.get("name", "")
            msg = s.get("message") or s["status"]
            return f"{name}: {msg}" if name else msg
    return r["status"]


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    src = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else "wpt-report.html"
    area = sys.argv[3] if len(sys.argv) > 3 else "css"

    d = load(src)
    results = d["results"]

    broken = []
    pass_sub = fail_sub = 0
    file_status = Counter()
    for r in results:
        subs = r.get("subtests", [])
        sp = sum(1 for s in subs if s["status"] in GOOD_SUB)
        sf = len(subs) - sp
        pass_sub += sp
        fail_sub += sf
        file_status[r["status"]] += 1
        is_broken = r["status"] not in GOOD_FILE or sf > 0
        if is_broken:
            broken.append((r, sp, sf))

    total = len(results)
    ok_files = total - len(broken)
    # Sort: worst status first, then by failing-subtest count desc, then name.
    order = {"CRASH": 0, "ERROR": 1, "TIMEOUT": 2, "FAIL": 3, "OK": 4, "PASS": 5}
    broken.sort(key=lambda t: (order.get(t[0]["status"], 9), -t[2], t[0]["test"]))

    total_sub = pass_sub + fail_sub
    pct_files = (ok_files / total * 100) if total else 0
    rev = d.get("run_info", {}).get("revision", "")[:10]

    rows = []
    for r, sp, sf in broken:
        st = r["status"]
        cls = {"OK": "fail", "FAIL": "fail", "TIMEOUT": "timeout",
               "ERROR": "error", "CRASH": "error"}.get(st, "fail")
        badge = "FAIL" if st == "OK" else st
        det = html.escape(first_failure(r))
        if len(det) > 300:
            det = det[:300] + "…"
        name = html.escape(r["test"].lstrip("/"))
        rows.append(
            f"<tr class=fail><td><span class='b {cls}'>{badge}</span></td>"
            f"<td class=name>{name}<div class=det>{det}</div></td>"
            f"<td class=num>{sp}</td><td class=num bad>{sf}</td></tr>"
        )

    statline = " · ".join(f"{v} {k}" for k, v in sorted(file_status.items()))
    doc = f"""<!doctype html><html><head><meta charset=utf-8><title>WPT — {area} (broken)</title><style>
:root{{color-scheme:light dark}}
body{{font-family:system-ui,sans-serif;margin:0;background:#fff;color:#1a1a1a}}
header{{padding:28px 32px;border-bottom:1px solid #ddd}}
h1{{margin:0 0 4px;font-size:22px}}
.sub{{color:#666;font-size:14px}}
.score{{font-size:56px;font-weight:700;margin:14px 0 6px}}
.score small{{font-size:22px;color:#666;font-weight:400}}
.track{{height:10px;border-radius:5px;background:#eee;overflow:hidden;max-width:520px}}
.fill{{height:100%;background:linear-gradient(90deg,#d33,#e90,#2a2);width:{pct_files:.1f}%}}
.meta{{margin-top:10px;color:#555;font-size:13px}}
table{{border-collapse:collapse;width:100%;font-size:13px}}
td,th{{padding:7px 12px;border-bottom:1px solid #eee;text-align:left;vertical-align:top}}
th{{position:sticky;top:0;background:#fafafa;font-size:12px;color:#666}}
.num{{text-align:right;font-variant-numeric:tabular-nums;width:60px}}
.num.bad{{color:#d33;font-weight:600}}
.name{{font-family:ui-monospace,monospace;color:#222}}
.det{{color:#b00;font-size:12px;margin-top:3px;font-family:ui-monospace,monospace}}
.b{{display:inline-block;padding:2px 8px;border-radius:4px;font-size:11px;font-weight:700;color:#fff}}
.b.pass{{background:#2a2}} .b.fail{{background:#d33}} .b.timeout{{background:#999}} .b.error{{background:#a0a}}
tr.pass td.name{{color:#444}}
@media (prefers-color-scheme: dark) {{
  body{{background:#15171a;color:#e6e6e6}}
  header{{border-bottom-color:#2a2d31}}
  .sub,.meta,.score small{{color:#9aa0a6}}
  .track{{background:#2a2d31}}
  td,th{{border-bottom-color:#23262a}}
  th{{background:#1b1e22;color:#9aa0a6}}
  .name{{color:#cfd3d7}} tr.pass td.name{{color:#8b9096}}
  .num.bad{{color:#ff6b6b}} .det{{color:#ff8a8a}}
}}
</style></head><body>
<header>
<h1>Web Platform Tests — <code>{area}</code> · broken files</h1>
<div class=sub>run against our own engine via the real <code>wpt run</code> WebDriver harness{(' · rev ' + rev) if rev else ''}</div>
<div class=score>{pct_files:.1f}% <small>{ok_files} / {total} files fully pass</small></div>
<div class=track><div class=fill></div></div>
<div class=meta>{len(broken)} broken files · {total} files ran · {fail_sub} failing / {total_sub} subtests · {statline}</div>
</header>
<table><thead><tr><th>Status</th><th>Test</th><th class=num>Pass</th><th class=num>Fail</th></tr></thead>
<tbody>
{chr(10).join(rows)}
</tbody></table>
</body></html>"""

    with open(out, "w") as f:
        f.write(doc)
    print(f"wrote {out}: {len(broken)} broken of {total} files")


if __name__ == "__main__":
    main()
