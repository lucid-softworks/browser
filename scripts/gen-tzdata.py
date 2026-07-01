#!/usr/bin/env python3
"""Generate crates/lumen/src/tzdata.rs from the system IANA time-zone database.

Parses the compiled TZif (v2, 64-bit) files under $ZONEINFO (default
/usr/share/zoneinfo) into per-zone UTC-offset transition tables, and derives the
Link (alias -> canonical) map by grouping zones with identical compiled contents
against the canonical names in zone1970.tab. No network access is required.

Usage: [ZONEINFO=/path/to/zoneinfo] python3 scripts/gen-tzdata.py
"""
import os, struct, hashlib

ZONEINFO = os.environ.get("ZONEINFO", "/usr/share/zoneinfo")

def parse_tzif(data):
    """Return (transitions[(epoch_sec, utoff_sec)], initial_utoff_sec)."""
    if data[:4] != b"TZif":
        return None
    version = data[4:5]
    def read_header(off):
        # magic(4) ver(1) reserved(15) then 6 uint32
        counts = struct.unpack(">6I", data[off + 20:off + 44])
        return counts, off + 44
    (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt), off = read_header(0)
    # Skip the v1 (32-bit) data block to reach the v2 block.
    v1_size = (timecnt * 5 + typecnt * 6 + charcnt
               + leapcnt * 8 + isstdcnt + isutcnt)
    if version in (b"2", b"3"):
        off2 = off + v1_size
        (isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt), off = read_header(off2)
        time_size = 8
    else:
        time_size = 4  # v1-only file
    times = []
    p = off
    for _ in range(timecnt):
        (t,) = struct.unpack(">q" if time_size == 8 else ">i", data[p:p + time_size])
        times.append(t)
        p += time_size
    type_idx = list(data[p:p + timecnt]); p += timecnt
    ttinfos = []
    for _ in range(typecnt):
        utoff, isdst, desig = struct.unpack(">ibB", data[p:p + 6])
        ttinfos.append(utoff)
        p += 6
    # Initial offset: the first non-DST type, else type 0 (per RFC 8536 guidance).
    initial = ttinfos[0] if ttinfos else 0
    transitions = []
    last = None
    for t, ti in zip(times, type_idx):
        off_s = ttinfos[ti] if ti < len(ttinfos) else 0
        if off_s != last:
            transitions.append((t, off_s))
            last = off_s
    return transitions, initial

def zone_files():
    skip_top = {"right", "posix", "SystemV", "Etc"}
    out = []
    for root, dirs, files in os.walk(ZONEINFO):
        rel_root = os.path.relpath(root, ZONEINFO)
        if rel_root != "." and rel_root.split(os.sep)[0] in skip_top:
            continue
        for f in files:
            if f.endswith(".tab") or f in ("leapseconds", "tzdata.zi", "+VERSION", "leap-seconds.list"):
                continue
            path = os.path.join(root, f)
            name = os.path.relpath(path, ZONEINFO)
            with open(path, "rb") as fh:
                data = fh.read()
            if data[:4] != b"TZif":
                continue
            out.append((name.replace(os.sep, "/"), data))
    # Include the Etc/* zones (fixed offsets) and UTC explicitly.
    etc_dir = os.path.join(ZONEINFO, "Etc")
    if os.path.isdir(etc_dir):
        for f in os.listdir(etc_dir):
            path = os.path.join(etc_dir, f)
            if not os.path.isfile(path):
                continue
            with open(path, "rb") as fh:
                data = fh.read()
            if data[:4] == b"TZif":
                out.append(("Etc/" + f, data))
    return out

def read_canonical():
    """Canonical zone names from zone1970.tab / zone.tab."""
    canon = set()
    for tab in ("zone1970.tab", "zone.tab"):
        path = os.path.join(ZONEINFO, tab)
        if not os.path.exists(path):
            continue
        for line in open(path):
            if line.startswith("#") or not line.strip():
                continue
            cols = line.rstrip("\n").split("\t")
            if len(cols) >= 3:
                canon.add(cols[2])
    return canon

def main():
    files = zone_files()
    canonical_set = read_canonical()
    # Etc/UTC and UTC are always canonical anchors.
    canonical_set |= {"UTC", "Etc/UTC", "Etc/GMT"}

    by_hash = {}
    parsed = {}
    for name, data in files:
        h = hashlib.sha1(data).hexdigest()
        by_hash.setdefault(h, []).append(name)
        parsed[name] = parse_tzif(data)

    # Choose a canonical representative per content group.
    links = {}       # alias -> canonical
    canon_zones = {} # name -> (transitions, initial)
    for h, names in by_hash.items():
        names_sorted = sorted(names)
        # "UTC" is Temporal's canonical id for the UTC group.
        if "UTC" in names:
            rep = "UTC"
        else:
            rep = next((n for n in names_sorted if n in canonical_set), None)
        if rep is None:
            # No tab entry: prefer a name without legacy prefixes.
            rep = next((n for n in names_sorted if "/" in n and not n.startswith(("US/", "Brazil/", "Canada/", "Chile/", "Mexico/", "Australia/", "Etc/"))), names_sorted[0])
        canon_zones[rep] = parsed[rep] or ([], 0)
        for n in names_sorted:
            if n != rep:
                links[n] = rep

    # Emit.
    o = []
    o.append("//! IANA time-zone offset tables — GENERATED by scripts/gen-tzdata.py. DO NOT EDIT.")
    o.append("//!")
    o.append("//! Each zone lists its UTC-offset transitions as (epoch_seconds, utc_offset_seconds),")
    o.append("//! sorted ascending; `initial` is the offset before the first transition.")
    o.append("")
    o.append("pub struct Zone {")
    o.append("    pub name: &'static str,")
    o.append("    pub initial: i32,")
    o.append("    pub transitions: &'static [(i64, i32)],")
    o.append("}")
    o.append("")
    canon_sorted = sorted(canon_zones)
    for name in canon_sorted:
        trans, initial = canon_zones[name]
        ident = "TZ_" + "".join("P" if c == "+" else "M" if c == "-" else (c if c.isalnum() else "_") for c in name).upper()
        parts = ", ".join(f"({t},{off})" for t, off in trans)
        o.append(f"static {ident}: &[(i64, i32)] = &[{parts}];")
    o.append("")
    o.append("/// Canonical zones, sorted by name.")
    o.append("pub static ZONES: &[Zone] = &[")
    for name in canon_sorted:
        _, initial = canon_zones[name]
        ident = "TZ_" + "".join("P" if c == "+" else "M" if c == "-" else (c if c.isalnum() else "_") for c in name).upper()
        o.append(f'    Zone {{ name: "{name}", initial: {initial}, transitions: {ident} }},')
    o.append("];")
    o.append("")
    o.append("/// (alias, canonical) links, sorted by alias.")
    o.append("pub static LINKS: &[(&str, &str)] = &[")
    for alias in sorted(links):
        o.append(f'    ("{alias}", "{links[alias]}"),')
    o.append("];")
    o.append("")

    dest = "crates/lumen/src/tzdata.rs"
    open(dest, "w").write("\n".join(o) + "\n")
    print(f"wrote {dest}: {len(canon_sorted)} zones, {len(links)} links")

if __name__ == "__main__":
    main()
