#!/usr/bin/env python3
"""Generate crates/lumen/src/umalqura.rs from the ICU Umm al-Qura tables.

The Umm al-Qura calendar (islamic-umalqura) is table-based for AH years 1300-1600: each year has a
12-bit month-length pattern (bit set = 30-day month) and a year-start day computed from a
least-squares linear estimate plus a small per-year correction. This mirrors ICU's islamcal.cpp so
Temporal.PlainDate etc. match the reference implementation. Outside 1300-1600, callers fall back to
the tabular islamic-civil calendar.

Source: unicode-org/icu icu4c/source/i18n/islamcal.cpp (fetched over the network, like the tz DB and
Unicode property codegen). No external Python deps.
"""
import re
import sys
import urllib.request

URL = "https://raw.githubusercontent.com/unicode-org/icu/main/icu4c/source/i18n/islamcal.cpp"
CIVIL_EPOC = 1948440   # Julian day number of islamic 1-1-1 (Friday epoch)
JD_1970 = 2440588      # JDN of 1970-01-01
EPOCH_OFFSET = CIVIL_EPOC - JD_1970  # add to (yearStart + monthoff + day-1) → epoch_days


def fetch(path):
    if len(sys.argv) > 1:
        return open(sys.argv[1]).read()
    return urllib.request.urlopen(URL, timeout=60).read().decode()


def main():
    src = fetch(URL)
    # Strip `//` line comments — the data uses trailing `//* 1597-1600 */ "...", };` comments whose
    # `};` would otherwise close the array match early.
    src = "\n".join(re.sub(r"//.*$", "", line) for line in src.splitlines())

    # Month-length bit patterns: every 0x.... literal inside the UMALQURA_MONTHLENGTH[] array.
    m = re.search(r"UMALQURA_MONTHLENGTH\[\]\s*=\s*\{(.*?)\};", src, re.S)
    months = [int(x, 16) for x in re.findall(r"0x[0-9A-Fa-f]+", m.group(1))]

    # Per-year start correction (small signed ints).
    f = re.search(r"umAlQuraYrStartEstimateFix\[\]\s*=\s*\{(.*?)\};", src, re.S)
    fix = [int(x) for x in re.findall(r"-?\d+", f.group(1))]

    n = 1600 - 1300 + 1
    assert len(months) == n, f"month table {len(months)} != {n}"
    assert len(fix) == n, f"fix table {len(fix)} != {n}"

    # Year start (days from the Hijri origin) per ICU's rounded least-squares fit.
    year_start = []
    for y in range(n):
        est = int(354.36720 * y + 460322.05 + 0.5)
        year_start.append(est + fix[y])

    # Sanity: umalqura 1445-01-01 should be 2023-07-19 (a widely cited new year).
    def days_from_civil(y, mo, d):
        y -= mo <= 2
        era = (y if y >= 0 else y - 399) // 400
        yoe = y - era * 400
        doy = (153 * (mo - 3 if mo > 2 else mo + 9) + 2) // 5 + d - 1
        doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
        return era * 146097 + doe - 719468
    got = year_start[1445 - 1300] + EPOCH_OFFSET
    want = days_from_civil(2023, 7, 19)
    assert got == want, f"1445-01-01 epoch_days {got} != {want} (2023-07-19)"

    out = []
    out.append("//! Umm al-Qura (islamic-umalqura) month-length + year-start tables for AH 1300-1600,")
    out.append("//! generated from ICU's islamcal.cpp by scripts/gen-umalqura.py. DO NOT EDIT.\n")
    out.append("pub const YEAR_START: i32 = 1300;")
    out.append("pub const YEAR_END: i32 = 1600;")
    out.append(f"/// Added to (year-start + month-offset + day-1) to get epoch-days (from 1970-01-01).")
    out.append(f"pub const EPOCH_OFFSET: i64 = {EPOCH_OFFSET};\n")
    out.append("/// 12-bit month-length pattern per year (bit `1<<(11-month0)` set → 30-day month).")
    out.append(f"pub const MONTH_LENGTHS: [u16; {n}] = [")
    for i in range(0, n, 12):
        out.append("    " + " ".join(f"0x{v:04X}," for v in months[i:i + 12]))
    out.append("];\n")
    out.append("/// Days from the Hijri origin to the start of each year (ICU least-squares fit).")
    out.append(f"pub const YEAR_STARTS: [i64; {n}] = [")
    for i in range(0, n, 12):
        out.append("    " + " ".join(f"{v}," for v in year_start[i:i + 12]))
    out.append("];")

    open("crates/lumen/src/umalqura.rs", "w").write("\n".join(out) + "\n")
    print(f"wrote crates/lumen/src/umalqura.rs ({n} years, epoch_offset={EPOCH_OFFSET})")


if __name__ == "__main__":
    main()
