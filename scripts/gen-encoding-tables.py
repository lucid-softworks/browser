#!/usr/bin/env python3
"""Generate crates/wurl/src/encoding_tables.rs — reverse (encode-direction) tables for the legacy
text encodings the URL query encoder needs (WHATWG Encoding Standard).

Reads the index-*.txt files from $ENC_DIR (or fetches them from encoding.spec.whatwg.org).
"""
import os, sys, urllib.request

ENC = os.environ.get("ENC_DIR")

def read(name):
    if ENC:
        return open(os.path.join(ENC, name)).read()
    return urllib.request.urlopen(f"https://encoding.spec.whatwg.org/{name}", timeout=60).read().decode()

def parse_index(name):
    """Return list of (pointer, code_point)."""
    out = []
    for line in read(name).splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        # Format: <pointer> <0xCODEPOINT> [name...]; ignore anything that doesn't start with digits.
        if len(parts) < 2 or not parts[0].isdigit():
            continue
        out.append((int(parts[0]), int(parts[1], 16)))
    return out

def reverse(name, lowest=True, exclude=None):
    """code point -> pointer, choosing the lowest pointer; `exclude(ptr)` drops pointers."""
    m = {}
    for ptr, cp in parse_index(name):
        if exclude and exclude(ptr):
            continue
        if cp not in m or ptr < m[cp]:
            m[cp] = ptr
    return sorted(m.items())

out = ["//! Generated reverse (encode-direction) tables for the WHATWG Encoding Standard legacy",
       "//! encodings. Regenerate with scripts/gen-encoding-tables.py.",
       "#![allow(clippy::unreadable_literal)]", ""]

def emit_cp_ptr(name, pairs):
    out.append(f"/// (code point, pointer) sorted by code point.")
    out.append(f"pub(crate) static {name}: &[(u32, u16)] = &[")
    out.extend(f"    ({cp},{ptr})," for cp, ptr in pairs)
    out.append("];")

# Single-byte: (code point, byte). The index maps index i (0..127) to a code point for byte 0x80+i.
def emit_single(name, idxname):
    pairs = {}
    for ptr, cp in parse_index(idxname):
        b = ptr + 0x80
        if cp not in pairs or b < pairs[cp]:
            pairs[cp] = b
    out.append(f"/// (code point, byte) for the 0x80..0xFF half of a single-byte encoding.")
    out.append(f"pub(crate) static {name}: &[(u32, u8)] = &[")
    out.extend(f"    ({cp},{b})," for cp, b in sorted(pairs.items()))
    out.append("];")

emit_single("WINDOWS_1252", "index-windows-1252.txt")
emit_single("ISO_8859_2", "index-iso-8859-2.txt")
emit_cp_ptr("EUC_KR", reverse("index-euc-kr.txt"))
emit_cp_ptr("BIG5", reverse("index-big5.txt", exclude=lambda p: p < (0xA1 - 0x81) * 157))
emit_cp_ptr("JIS0208", reverse("index-jis0208.txt"))
emit_cp_ptr("SHIFT_JIS", reverse("index-jis0208.txt", exclude=lambda p: 8272 <= p <= 8835))
emit_cp_ptr("GB18030", reverse("index-gb18030.txt"))

# gb18030 four-byte ranges: (pointer, code point) sorted by code point for the encoder.
ranges = sorted(parse_index("index-gb18030-ranges.txt"), key=lambda t: t[1])
out.append("/// gb18030 four-byte ranges as (code point, pointer) sorted by code point.")
out.append("pub(crate) static GB18030_RANGES: &[(u32, u32)] = &[")
out.extend(f"    ({cp},{ptr})," for ptr, cp in ranges)
out.append("];")

open("crates/wurl/src/encoding_tables.rs", "w").write("\n".join(out) + "\n")
print("wrote encoding_tables.rs")
