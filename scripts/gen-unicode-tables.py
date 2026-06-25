#!/usr/bin/env python3
"""Generate crates/wurl/src/unicode_tables.rs from the Unicode Character Database.

Downloads (or reads from $UCD_DIR) IdnaMappingTable.txt, UnicodeData.txt,
CompositionExclusions.txt and DerivedNormalizationProps.txt for a given Unicode
version and emits the UTS-46 mapping table plus the NFC decomposition/composition
/combining-class tables our from-scratch URL host parser uses.

Usage: UCD_DIR=/path/to/ucd python3 scripts/gen-unicode-tables.py [version]
(without UCD_DIR it fetches from www.unicode.org for the given version, default 16.0.0)
"""
import os, sys, urllib.request

VERSION = sys.argv[1] if len(sys.argv) > 1 else "16.0.0"
UCD = os.environ.get("UCD_DIR")

def read(name, idna=False, extracted=False):
    if UCD:
        return open(os.path.join(UCD, os.path.basename(name))).read()
    if idna:
        base = f"https://www.unicode.org/Public/idna/{VERSION}/"
    elif extracted:
        base = f"https://www.unicode.org/Public/{VERSION}/ucd/extracted/"
    else:
        base = f"https://www.unicode.org/Public/{VERSION}/ucd/"
    return urllib.request.urlopen(base + name, timeout=60).read().decode()

def rust_str(s):
    out = '"'
    for ch in s:
        o = ord(ch)
        if ch == '\\': out += '\\\\'
        elif ch == '"': out += '\\"'
        elif 0x20 <= o < 0x7f: out += ch
        else: out += '\\u{%x}' % o
    return out + '"'

# UTS-46 mapping (non-transitional, UseSTD3=false).
rows = []
for line in read("IdnaMappingTable.txt", idna=True).splitlines():
    line = line.split('#')[0].strip()
    if not line: continue
    p = [x.strip() for x in line.split(';')]
    s, e = (int(x, 16) for x in p[0].split('..')) if '..' in p[0] else (int(p[0], 16),) * 2
    st = p[1]
    if st in ('valid', 'deviation', 'disallowed_STD3_valid'): kind, m = 0, ''
    elif st in ('mapped', 'disallowed_STD3_mapped'):
        kind = 1; m = ''.join(chr(int(x, 16)) for x in p[2].split()) if len(p) > 2 and p[2] else ''
    elif st == 'ignored': kind, m = 2, ''
    else: kind, m = 3, ''
    rows.append([s, e, kind, m])
merged = []
for s, e, k, m in rows:
    if merged and merged[-1][2] == k and k != 1 and merged[-1][1] + 1 == s and merged[-1][3] == m:
        merged[-1][1] = e
    else: merged.append([s, e, k, m])

# NFC tables + General_Category=Mark (Mn/Mc/Me) for the IDNA leading-combining-mark check.
ccc, decomp, marks = {}, {}, []
for line in read("UnicodeData.txt").splitlines():
    f = line.split(';')
    cp = int(f[0], 16)
    if int(f[3]): ccc[cp] = int(f[3])
    if f[5] and not f[5].startswith('<'): decomp[cp] = [int(x, 16) for x in f[5].split()]
    if f[2] in ('Mn', 'Mc', 'Me'): marks.append(cp)
mmerged = []
for cp in sorted(marks):
    if mmerged and mmerged[-1][1] + 1 == cp: mmerged[-1][1] = cp
    else: mmerged.append([cp, cp])
def full(cp):
    return [y for x in decomp[cp] for y in full(x)] if cp in decomp else [cp]
fulldecomp = {cp: full(cp) for cp in decomp}
excl = set()
for line in read("DerivedNormalizationProps.txt").splitlines():
    raw = line.split('#')[0].strip()
    if not raw: continue
    p = [x.strip() for x in raw.split(';')]
    if len(p) >= 2 and p[1] == 'Full_Composition_Exclusion':
        s, e = (int(x, 16) for x in p[0].split('..')) if '..' in p[0] else (int(p[0], 16),) * 2
        excl.update(range(s, e + 1))
compose = {(d[0], d[1]): cp for cp, d in decomp.items() if len(d) == 2 and cp not in excl}

# Derived Joining_Type (for the IDNA ContextJ rules). Default (unlisted) is U.
JT = {'U': 0, 'L': 1, 'R': 2, 'D': 3, 'C': 4, 'T': 5}
joining = []
for line in read("DerivedJoiningType.txt", extracted=True).splitlines():
    line = line.split('#')[0].strip()
    if not line: continue
    p = [x.strip() for x in line.split(';')]
    s, e = (int(x, 16) for x in p[0].split('..')) if '..' in p[0] else (int(p[0], 16),) * 2
    t = JT.get(p[1], 0)
    if t != 0: joining.append([s, e, t])
joining.sort()
jmerged = []
for s, e, t in joining:
    if jmerged and jmerged[-1][2] == t and jmerged[-1][1] + 1 == s:
        jmerged[-1][1] = e
    else: jmerged.append([s, e, t])

o = [f"//! Generated Unicode tables (UTS-46 mapping + NFC) — Unicode {VERSION}. Do not edit by hand;",
     "//! regenerate from the UCD with scripts/gen-unicode-tables.py.",
     "#![allow(clippy::unreadable_literal)]", "",
     "/// (start, end, kind) with kind 0=valid 1=mapped 2=ignored 3=disallowed; mapped targets in UTS46_MAP.",
     "pub(crate) static UTS46: &[(u32, u32, u8)] = &["]
o += [f"    ({s},{e},{k})," for s, e, k, m in merged]
o += ["];", "/// For a mapped range, its start code point -> replacement string (sorted by start).",
      "pub(crate) static UTS46_MAP: &[(u32, &str)] = &["]
o += [f"    ({s},{rust_str(m)})," for s, e, k, m in merged if k == 1]
o += ["];", "/// Canonical combining class for code points with ccc != 0 (sorted).",
      "pub(crate) static CCC: &[(u32, u8)] = &["]
o += [f"    ({cp},{ccc[cp]})," for cp in sorted(ccc)]
o += ["];", "/// Full canonical decomposition (recursively expanded), sorted by code point.",
      "pub(crate) static DECOMP: &[(u32, &str)] = &["]
o += [f"    ({cp},{rust_str(''.join(chr(x) for x in fulldecomp[cp]))})," for cp in sorted(fulldecomp)]
o += ["];", "/// Canonical composition: (first, second) -> composed, sorted.",
      "pub(crate) static COMPOSE: &[(u32, u32, u32)] = &["]
o += [f"    ({a},{b},{compose[(a, b)]})," for a, b in sorted(compose)]
o += ["];", "/// Joining_Type ranges (1=L 2=R 3=D 4=C 5=T; unlisted=U), for the IDNA ContextJ rules.",
      "pub(crate) static JOINING: &[(u32, u32, u8)] = &["]
o += [f"    ({s},{e},{t})," for s, e, t in jmerged]
o += ["];", "/// Code-point ranges with General_Category in {Mn, Mc, Me} (combining marks).",
      "pub(crate) static MARK: &[(u32, u32)] = &["]
o += [f"    ({s},{e})," for s, e in mmerged]
o += ["];"]

# Bidi_Class (for the IDNA CheckBidi rule, RFC 5893). Encode the classes the rule references.
BC = {'L': 1, 'R': 2, 'AL': 3, 'AN': 4, 'EN': 5, 'ES': 6, 'CS': 7, 'ET': 8, 'ON': 9, 'BN': 10, 'NSM': 11}
bidi = []
for line in read("DerivedBidiClass.txt", extracted=True).splitlines():
    line = line.split('#')[0].strip()
    if not line: continue
    p = [x.strip() for x in line.split(';')]
    if len(p) < 2 or p[1] not in BC: continue
    s, e = (int(x, 16) for x in p[0].split('..')) if '..' in p[0] else (int(p[0], 16),) * 2
    bidi.append([s, e, BC[p[1]]])
bidi.sort()
bmerged = []
for s, e, t in bidi:
    if bmerged and bmerged[-1][2] == t and bmerged[-1][1] + 1 == s: bmerged[-1][1] = e
    else: bmerged.append([s, e, t])
o += ["/// Bidi_Class ranges for the IDNA CheckBidi rule: 1=L 2=R 3=AL 4=AN 5=EN 6=ES 7=CS 8=ET 9=ON 10=BN 11=NSM.",
      "pub(crate) static BIDI: &[(u32, u32, u8)] = &["]
o += [f"    ({s},{e},{t})," for s, e, t in bmerged]
o += ["];"]
open("crates/wurl/src/unicode_tables.rs", "w").write("\n".join(o) + "\n")
print(f"wrote {len(merged)} UTS46 ranges, {len(fulldecomp)} decomp, {len(compose)} compose")
