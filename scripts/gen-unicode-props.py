#!/usr/bin/env python3
"""Generate crates/lumen/src/unicode_props.rs from the Unicode Character Database.

Downloads the UCD (version pinned below) and emits Rust tables of inclusive code-point ranges for
the Unicode properties used by RegExp `\\p{...}` property escapes: General_Category, Script,
Script_Extensions, and the binary properties. Every accepted spelling/alias of a property and value
maps to the same range table (loose matching: case-insensitive, `_`/`-`/spaces ignored), so the
matcher in regex.rs is a single lookup.

Run from the repo root:  python3 scripts/gen-unicode-props.py
"""
import os
import urllib.request

VERSION = "17.0.0"
BASE = f"https://www.unicode.org/Public/{VERSION}/ucd"
CACHE = "/tmp/ucd17"
OUT = "crates/lumen/src/unicode_props.rs"
MAX_CP = 0x10FFFF

# General_Category groups (first-letter unions), fixed by Unicode.
GC_GROUPS = {
    "L": ["Lu", "Ll", "Lt", "Lm", "Lo"],
    "LC": ["Lu", "Ll", "Lt"],
    "M": ["Mn", "Mc", "Me"],
    "N": ["Nd", "Nl", "No"],
    "P": ["Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po"],
    "S": ["Sm", "Sc", "Sk", "So"],
    "Z": ["Zs", "Zl", "Zp"],
    "C": ["Cc", "Cf", "Cs", "Co", "Cn"],
}


def fetch(path):
    cache = os.path.join(CACHE, os.path.basename(path))
    if os.path.exists(cache):
        return open(cache, encoding="utf-8").read()
    os.makedirs(CACHE, exist_ok=True)
    data = urllib.request.urlopen(f"{BASE}/{path}", timeout=120).read().decode("utf-8")
    open(cache, "w").write(data)
    return data


def parse_ranges(text):
    """`value -> [(lo, hi), ...]` from a `range ; value # comment` file (multi-value lines split)."""
    d = {}
    for line in text.splitlines():
        line = line.split("#")[0].strip()
        if not line:
            continue
        parts = [p.strip() for p in line.split(";")]
        rng = parts[0]
        lo, hi = (rng.split("..") + [rng])[:2] if ".." in rng else (rng, rng)
        lo, hi = int(lo, 16), int(hi, 16)
        for val in parts[1].split():  # ScriptExtensions lists multiple scripts per line
            d.setdefault(val, []).append((lo, hi))
    return d


def merge(ranges):
    out = []
    for lo, hi in sorted(ranges):
        if out and lo <= out[-1][1] + 1:
            out[-1] = (out[-1][0], max(out[-1][1], hi))
        else:
            out.append((lo, hi))
    return out


def complement(ranges):
    out, prev = [], 0
    for lo, hi in merge(ranges):
        if lo > prev:
            out.append((prev, lo - 1))
        prev = hi + 1
    if prev <= MAX_CP:
        out.append((prev, MAX_CP))
    return out


def loose(s):
    return s.lower().replace("_", "").replace("-", "").replace(" ", "")


def main():
    gc = parse_ranges(fetch("extracted/DerivedGeneralCategory.txt"))
    scripts = parse_ranges(fetch("Scripts.txt"))
    scx = parse_ranges(fetch("ScriptExtensions.txt"))
    binprops = {}
    for f in ("PropList.txt", "DerivedCoreProperties.txt", "emoji/emoji-data.txt",
              "extracted/DerivedBinaryProperties.txt"):
        for k, v in parse_ranges(fetch(f)).items():
            binprops.setdefault(k, []).extend(v)
    # DerivedNormalizationProps lists some binary properties (single value field) alongside
    # multi-valued ones (`NFD_QC ; N`); keep only the binary `range ; Name` rows.
    for line in fetch("DerivedNormalizationProps.txt").splitlines():
        line = line.split("#")[0].strip()
        if not line:
            continue
        parts = [p.strip() for p in line.split(";")]
        if len(parts) == 2:
            rng = parts[0]
            lo, hi = (rng.split("..") + [rng])[:2] if ".." in rng else (rng, rng)
            binprops.setdefault(parts[1], []).append((int(lo, 16), int(hi, 16)))

    # Property + property-value aliases (short/long/extra spellings).
    # Map EVERY spelling (short, long, extras) to the full list, so a lookup by any name works.
    prop_aliases = {}  # any spelling -> [all spellings]
    for line in fetch("PropertyAliases.txt").splitlines():
        line = line.split("#")[0].strip()
        if line:
            names = [p.strip() for p in line.split(";")]
            for n in names:
                prop_aliases[n] = names
    val_aliases = {}  # (prop, any value spelling) -> [all value spellings]
    for line in fetch("PropertyValueAliases.txt").splitlines():
        line = line.split("#")[0].strip()
        if line:
            parts = [p.strip() for p in line.split(";")]
            prop, spellings = parts[0], parts[1:]
            for sp in spellings:
                val_aliases[(prop, sp)] = spellings

    def aliases_for_prop(short):
        return prop_aliases.get(short, [short])

    def aliases_for_value(prop_short, value_short):
        return val_aliases.get((prop_short, value_short), [value_short])

    # Build: canonical-key -> merged ranges. A key is `prop=value` (loose) or a lone `name`.
    tables = {}  # name -> list of keys (filled after we assign table ids)
    by_ranges = {}  # frozenset(ranges) -> table id
    table_list = []  # id -> ranges
    keymap = {}  # loose key -> table id

    def add_table(ranges):
        ranges = tuple(merge(ranges))
        if ranges not in by_ranges:
            by_ranges[ranges] = len(table_list)
            table_list.append(ranges)
        return by_ranges[ranges]

    def bind(keys, ranges):
        tid = add_table(ranges)
        for k in keys:
            keymap[loose(k)] = tid

    # --- General_Category (lone value, and gc=value / General_Category=value) ---
    gc_all = dict(gc)
    for grp, members in GC_GROUPS.items():
        merged = []
        for m in members:
            merged += gc_all.get(m, [])
        gc_all[grp] = merged
    for val, ranges in gc_all.items():
        spellings = aliases_for_value("gc", val)
        keys = list(spellings)
        for pn in aliases_for_prop("gc"):
            for vs in spellings:
                keys.append(f"{pn}={vs}")
        bind(keys, ranges)

    # NOTE: Any / ASCII / Assigned are UTS#18 properties but NOT valid ECMAScript `\p{}` names —
    # `/\p{Any}/u` must be a SyntaxError — so they are deliberately omitted.

    # --- Script (Script=value only) ---
    # The `Unknown` (Zzzz) script covers every code point in no explicit script.
    all_script_ranges = []
    for ranges in scripts.values():
        all_script_ranges += ranges
    scripts = dict(scripts)
    scripts["Unknown"] = complement(all_script_ranges)
    for val, ranges in scripts.items():
        keys = []
        for pn in aliases_for_prop("sc"):
            for vs in aliases_for_value("sc", val):
                keys.append(f"{pn}={vs}")
        bind(keys, ranges)

    # --- Script_Extensions (scx=value); a script's scx includes its Script ranges) ---
    scx_all = {}
    for val, ranges in scripts.items():
        scx_all.setdefault(val, []).extend(ranges)
    for val, ranges in scx.items():
        # ScriptExtensions uses script short codes; map to the canonical long value.
        long = aliases_for_value("sc", val)[-1] if (("sc", val) in val_aliases) else val
        scx_all.setdefault(long, []).extend(ranges)
    for val, ranges in scx_all.items():
        keys = []
        for pn in aliases_for_prop("scx"):
            for vs in aliases_for_value("sc", val) + [val]:
                keys.append(f"{pn}={vs}")
        bind(keys, ranges)

    # --- Binary properties (lone name) ---
    for val, ranges in binprops.items():
        keys = list(aliases_for_prop(val)) or [val]
        bind(keys, ranges)

    # --- Emit Rust ---
    lines = [
        "//! GENERATED by scripts/gen-unicode-props.py from the Unicode "
        f"{VERSION} UCD. Do not edit.",
        "//!",
        "//! Code-point range tables for RegExp `\\p{...}` property escapes. `lookup` canonicalizes a",
        "//! property name/value (loose matching) and returns the sorted inclusive ranges, or None.",
        "",
        "type Ranges = &'static [(u32, u32)];",
        "",
    ]
    for tid, ranges in enumerate(table_list):
        body = ", ".join(f"({lo:#x},{hi:#x})" for lo, hi in ranges)
        lines.append(f"static T{tid}: Ranges = &[{body}];")
    lines.append("")
    lines.append("/// (loose key, ranges) pairs, sorted by key for binary search.")
    items = sorted(keymap.items())
    lines.append(f"static KEYS: &[(&str, Ranges)] = &[")
    for k, tid in items:
        assert all(c.isalnum() or c == "=" for c in k), f"unsafe key {k!r}"
        lines.append(f'    ("{k}", T{tid}),')
    lines.append("];")
    lines += [
        "",
        "fn canon(s: &str) -> String {",
        "    s.chars().filter(|c| *c != '_' && *c != '-' && *c != ' ').flat_map(|c| c.to_lowercase()).collect()",
        "}",
        "",
        "/// Ranges for `\\p{name}` (value None) or `\\p{name=value}`. None if unknown.",
        "pub fn lookup(name: &str, value: Option<&str>) -> Option<Ranges> {",
        "    let key = match value {",
        "        Some(v) => format!(\"{}={}\", canon(name), canon(v)),",
        "        None => canon(name),",
        "    };",
        "    KEYS.binary_search_by(|(k, _)| k.cmp(&key.as_str())).ok().map(|i| KEYS[i].1)",
        "}",
        "",
    ]
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    open(OUT, "w").write("\n".join(lines))
    print(f"wrote {OUT}: {len(table_list)} tables, {len(items)} keys")


if __name__ == "__main__":
    main()
