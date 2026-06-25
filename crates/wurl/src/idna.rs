//! From-scratch IDNA (UTS-46) domain-to-ASCII, built on the generated Unicode tables
//! ([`crate::unicode_tables`]): UTS-46 mapping, NFC normalization (canonical decomposition +
//! ordering + composition, including the algorithmic Hangul handling), and Punycode (RFC 3492). No
//! external `idna`/`url` dependency.
//!
//! Processing options match the WHATWG URL spec / WPT IDNA suites: non-transitional,
//! UseSTD3ASCIIRules=false, CheckHyphens=false, CheckBidi=true, CheckJoiners=true. We validate the
//! disallowed/NFC/leading-combining-mark/ContextJ/Bidi criteria.

use crate::unicode_tables::{BIDI, CCC, COMPOSE, DECOMP, JOINING, MARK, UTS46, UTS46_MAP};

fn is_mark(c: char) -> bool {
    let u = c as u32;
    let idx = MARK.partition_point(|&(_, end)| end < u);
    idx < MARK.len() && {
        let (start, end) = MARK[idx];
        u >= start && u <= end
    }
}

// Bidi_Class codes (see the BIDI table): 1=L 2=R 3=AL 4=AN 5=EN 6=ES 7=CS 8=ET 9=ON 10=BN 11=NSM.
fn bidi_class(c: char) -> u8 {
    let u = c as u32;
    let idx = BIDI.partition_point(|&(_, end, _)| end < u);
    if idx < BIDI.len() {
        let (start, end, t) = BIDI[idx];
        if u >= start && u <= end {
            return t;
        }
    }
    1 // default L (unlisted assigned code points are otherwise disallowed before this point)
}

/// IDNA CheckBidi rule (RFC 5893) for one label of a bidi domain.
fn label_bidi_ok(chars: &[char]) -> bool {
    let classes: Vec<u8> = chars.iter().map(|&c| bidi_class(c)).collect();
    let last_non_nsm = classes.iter().rev().find(|&&c| c != 11).copied();
    match classes[0] {
        // RTL label (starts R or AL).
        2 | 3 => {
            if !classes.iter().all(|&c| matches!(c, 2..=11)) {
                return false;
            }
            if !matches!(last_non_nsm, Some(2..=5)) {
                return false;
            }
            !(classes.contains(&5) && classes.contains(&4))
        }
        // LTR label (starts L).
        1 => {
            if !classes.iter().all(|&c| matches!(c, 1 | 5..=11)) {
                return false;
            }
            matches!(last_non_nsm, Some(1 | 5))
        }
        _ => false,
    }
}

const VIRAMA: u8 = 9;
// Joining_Type codes (see the generated JOINING table): 1=L 2=R 3=D 4=C 5=T; unlisted = U(0).
fn joining_type(c: char) -> u8 {
    let u = c as u32;
    let idx = JOINING.partition_point(|&(_, end, _)| end < u);
    if idx < JOINING.len() {
        let (start, end, t) = JOINING[idx];
        if u >= start && u <= end {
            return t;
        }
    }
    0
}

/// IDNA ContextJ rule for ZWNJ (U+200C): valid after a Virama, or inside an (L|D) T* _ T* (R|D)
/// joining sequence (RFC 5892 A.1).
fn zwnj_ok(chars: &[char], idx: usize) -> bool {
    if idx > 0 && ccc(chars[idx - 1]) == VIRAMA {
        return true;
    }
    let mut j = idx;
    while j > 0 && joining_type(chars[j - 1]) == 5 {
        j -= 1;
    }
    if j == 0 || !matches!(joining_type(chars[j - 1]), 1 | 3) {
        return false;
    }
    let mut k = idx + 1;
    while k < chars.len() && joining_type(chars[k]) == 5 {
        k += 1;
    }
    k < chars.len() && matches!(joining_type(chars[k]), 2 | 3)
}

// Hangul syllable composition constants (UAX #15).
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT; // 588
const S_COUNT: u32 = L_COUNT * N_COUNT; // 11172

enum Status {
    Valid,
    Mapped(&'static str),
    Ignored,
    Disallowed,
}

fn uts46_status(c: char) -> Status {
    let u = c as u32;
    let idx = UTS46.partition_point(|&(_, end, _)| end < u);
    if idx < UTS46.len() {
        let (start, end, kind) = UTS46[idx];
        if u >= start && u <= end {
            return match kind {
                0 => Status::Valid,
                1 => {
                    let mi = UTS46_MAP.partition_point(|&(s, _)| s < start);
                    Status::Mapped(UTS46_MAP[mi].1)
                }
                2 => Status::Ignored,
                _ => Status::Disallowed,
            };
        }
    }
    Status::Disallowed
}

fn ccc(c: char) -> u8 {
    let u = c as u32;
    match CCC.binary_search_by(|&(cp, _)| cp.cmp(&u)) {
        Ok(i) => CCC[i].1,
        Err(_) => 0,
    }
}

fn decompose(c: char, out: &mut Vec<char>) {
    let u = c as u32;
    if (S_BASE..S_BASE + S_COUNT).contains(&u) {
        let s = u - S_BASE;
        out.push(char::from_u32(L_BASE + s / N_COUNT).unwrap());
        out.push(char::from_u32(V_BASE + (s % N_COUNT) / T_COUNT).unwrap());
        let t = s % T_COUNT;
        if t != 0 {
            out.push(char::from_u32(T_BASE + t).unwrap());
        }
        return;
    }
    match DECOMP.binary_search_by(|&(cp, _)| cp.cmp(&u)) {
        Ok(i) => out.extend(DECOMP[i].1.chars()),
        Err(_) => out.push(c),
    }
}

fn canonical_order(chars: &mut [char]) {
    // Stable bubble of adjacent combining marks by combining class.
    if chars.len() < 2 {
        return;
    }
    let mut i = 1;
    while i < chars.len() {
        let a = ccc(chars[i - 1]);
        let b = ccc(chars[i]);
        if b != 0 && a != 0 && a > b {
            chars.swap(i - 1, i);
            if i > 1 {
                i -= 1;
                continue;
            }
        }
        i += 1;
    }
}

fn compose_pair(a: char, b: char) -> Option<char> {
    let (au, bu) = (a as u32, b as u32);
    // Hangul L + V
    if (L_BASE..L_BASE + L_COUNT).contains(&au) && (V_BASE..V_BASE + V_COUNT).contains(&bu) {
        let l = au - L_BASE;
        let v = bu - V_BASE;
        return char::from_u32(S_BASE + (l * V_COUNT + v) * T_COUNT);
    }
    // Hangul LV + T
    if (S_BASE..S_BASE + S_COUNT).contains(&au)
        && (au - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&bu)
    {
        return char::from_u32(au + (bu - T_BASE));
    }
    match COMPOSE.binary_search_by(|&(x, y, _)| (x, y).cmp(&(au, bu))) {
        Ok(i) => char::from_u32(COMPOSE[i].2),
        Err(_) => None,
    }
}

fn nfc(input: &str) -> String {
    let mut chars: Vec<char> = Vec::with_capacity(input.len());
    for c in input.chars() {
        decompose(c, &mut chars);
    }
    canonical_order(&mut chars);
    // Canonical composition.
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if ccc(c) == 0 {
            let mut composed = c;
            let mut last_class: i32 = -1;
            let mut remaining: Vec<char> = Vec::new();
            let mut j = i + 1;
            while j < chars.len() {
                let d = chars[j];
                let dcc = ccc(d) as i32;
                if last_class < dcc {
                    if let Some(x) = compose_pair(composed, d) {
                        composed = x;
                        j += 1;
                        continue;
                    }
                }
                if dcc == 0 {
                    break;
                }
                last_class = dcc;
                remaining.push(d);
                j += 1;
            }
            out.push(composed);
            out.extend(remaining);
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// A UTS-46 label is valid if it is NFC, doesn't begin with a combining mark, and every code point
/// has "valid" status (CheckHyphens/CheckBidi off, per the WPT options).
fn valid_label(chars: &[char], check_bidi: bool) -> bool {
    if chars.is_empty() {
        return false;
    }
    // A label must not begin with a combining mark (General_Category Mark).
    if is_mark(chars[0]) {
        return false;
    }
    if check_bidi && !label_bidi_ok(chars) {
        return false;
    }
    for (idx, &c) in chars.iter().enumerate() {
        // ZWNJ/ZWJ are valid only in their ContextJ join contexts (RFC 5892 A.1/A.2).
        if c == '\u{200c}' {
            if !zwnj_ok(chars, idx) {
                return false;
            }
        } else if c == '\u{200d}' {
            if idx == 0 || ccc(chars[idx - 1]) != VIRAMA {
                return false;
            }
        } else if !matches!(uts46_status(c), Status::Valid) {
            return false;
        }
    }
    let s: String = chars.iter().collect();
    nfc(&s) == s
}

/// UTS-46 ToASCII (the host parser's domain-to-ASCII step).
pub(crate) fn domain_to_ascii(domain: &str) -> Result<String, ()> {
    // 1. Map (and reject disallowed code points).
    let mut mapped = String::with_capacity(domain.len());
    for c in domain.chars() {
        match uts46_status(c) {
            Status::Valid => mapped.push(c),
            Status::Mapped(s) => mapped.push_str(s),
            Status::Ignored => {}
            Status::Disallowed => return Err(()),
        }
    }
    // 2. Normalize (NFC).
    let normalized = nfc(&mapped);
    // 3. Split into labels. For an ACE (xn--) label, decode + pre-validate the Punycode; `verbatim`
    //    holds the ASCII output for ASCII/ACE labels, `uni` the Unicode form used for validation.
    let mut labels: Vec<(Option<String>, Vec<char>)> = Vec::new();
    for label in normalized.split('.') {
        if let Some(rest) = label.strip_prefix("xn--") {
            if rest.is_empty() {
                return Err(());
            }
            let decoded = punycode_decode(rest).ok_or(())?;
            // An ACE label must decode to ≥1 non-ASCII code point and round-trip.
            if decoded.iter().all(char::is_ascii)
                || punycode_encode(&decoded).as_deref() != Some(rest)
            {
                return Err(());
            }
            labels.push((Some(label.to_string()), decoded));
        } else if label.is_ascii() {
            labels.push((Some(label.to_string()), label.chars().collect()));
        } else {
            labels.push((None, label.chars().collect()));
        }
    }
    // 4. A domain is a "bidi domain" if any label (in its Unicode form) has an R/AL/AN code point;
    //    CheckBidi then applies to every label.
    let check_bidi = labels
        .iter()
        .any(|(_, uni)| uni.iter().any(|&c| matches!(bidi_class(c), 2..=4)));
    // 5. Validate each non-empty label and assemble the ASCII output.
    let mut out = String::new();
    for (i, (verbatim, uni)) in labels.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        if uni.is_empty() {
            // Empty label (e.g. a trailing dot) — allowed.
            continue;
        }
        if !valid_label(uni, check_bidi) {
            return Err(());
        }
        match verbatim {
            Some(s) => out.push_str(s),
            None => {
                out.push_str("xn--");
                out.push_str(&punycode_encode(uni).ok_or(())?);
            }
        }
    }
    if out.is_empty() {
        return Err(());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Punycode (RFC 3492)
// ---------------------------------------------------------------------------------------------

const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;

fn adapt(mut delta: u32, num_points: u32, first_time: bool) -> u32 {
    delta = if first_time { delta / DAMP } else { delta / 2 };
    delta += delta / num_points;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + (((BASE - TMIN + 1) * delta) / (delta + SKEW))
}

fn punycode_encode(input: &[char]) -> Option<String> {
    fn digit_to_basic(d: u32) -> char {
        if d < 26 {
            (b'a' + d as u8) as char
        } else {
            (b'0' + (d - 26) as u8) as char
        }
    }
    let mut output = String::new();
    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let basics: Vec<u32> = input
        .iter()
        .map(|&c| c as u32)
        .filter(|&c| c < 0x80)
        .collect();
    let b = basics.len();
    for &c in &basics {
        output.push(char::from_u32(c)?);
    }
    if b > 0 {
        output.push('-');
    }
    let mut h = b as u32;
    let total = input.len() as u32;
    while h < total {
        let m = input.iter().map(|&c| c as u32).filter(|&c| c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(h + 1)?)?;
        n = m;
        for &cc in input {
            let c = cc as u32;
            if c < n {
                delta = delta.checked_add(1)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    output.push(digit_to_basic(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                output.push(digit_to_basic(q));
                bias = adapt(delta, h + 1, h == b as u32);
                delta = 0;
                h += 1;
            }
        }
        delta += 1;
        n += 1;
    }
    Some(output)
}

pub(crate) fn punycode_decode(input: &str) -> Option<Vec<char>> {
    fn basic_to_digit(c: u8) -> Option<u32> {
        match c {
            b'a'..=b'z' => Some((c - b'a') as u32),
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'0'..=b'9' => Some((c - b'0' + 26) as u32),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    let mut output: Vec<u32> = Vec::new();
    let (basic, rest) = match input.rfind('-') {
        Some(idx) => (&bytes[..idx], &bytes[idx + 1..]),
        None => (&bytes[..0], bytes),
    };
    for &b in basic {
        if b >= 0x80 {
            return None;
        }
        output.push(b as u32);
    }
    let mut n = INITIAL_N;
    let mut i: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut pos = 0usize;
    while pos < rest.len() {
        let oldi = i;
        let mut w = 1u32;
        let mut k = BASE;
        loop {
            if pos >= rest.len() {
                return None;
            }
            let digit = basic_to_digit(rest[pos])?;
            pos += 1;
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let out_len = output.len() as u32 + 1;
        bias = adapt(i - oldi, out_len, oldi == 0);
        n = n.checked_add(i / out_len)?;
        i %= out_len;
        char::from_u32(n)?;
        output.insert(i as usize, n);
        i += 1;
    }
    output.into_iter().map(char::from_u32).collect()
}
