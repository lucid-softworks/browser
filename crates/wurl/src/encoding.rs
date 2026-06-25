//! Legacy text-encoding *encoders* (WHATWG Encoding Standard), used for URL query encoding: when a
//! document's character encoding isn't UTF-8, the query component of a URL parsed in that document
//! is encoded with that encoding (the fragment/path stay UTF-8). Built on the generated reverse
//! tables in [`crate::encoding_tables`]. Encode-direction only — decoding lives in the HTML/engine
//! layer.

use crate::encoding_tables::{
    BIG5, EUC_KR, GB18030, GB18030_RANGES, ISO_8859_2, JIS0208, SHIFT_JIS, WINDOWS_1252,
};

/// Normalize an encoding label to one of the names we handle (a small subset of the Encoding
/// Standard's labels — enough for the document charsets the URL query tests use). Returns None for
/// UTF-8 / unknown (the caller then uses plain UTF-8 percent-encoding).
pub fn label(input: &str) -> Option<&'static str> {
    let l = input.trim().to_ascii_lowercase();
    Some(match l.as_str() {
        "big5" | "big5-hkscs" | "cn-big5" | "csbig5" | "x-x-big5" => "big5",
        "euc-kr" | "cseuckr" | "csksc56011987" | "iso-ir-149" | "korean" | "ks_c_5601-1987"
        | "ks_c_5601-1989" | "ksc5601" | "ksc_5601" | "windows-949" => "euc-kr",
        "gb18030" => "gb18030",
        "gbk" | "chinese" | "csgb2312" | "csiso58gb231280" | "gb2312" | "gb_2312"
        | "gb_2312-80" | "iso-ir-58" | "x-gbk" => "gb18030",
        "iso-2022-jp" | "csiso2022jp" => "iso-2022-jp",
        "shift_jis" | "csshiftjis" | "ms932" | "ms_kanji" | "shift-jis" | "sjis"
        | "windows-31j" | "x-sjis" => "shift_jis",
        "iso-8859-2" | "iso-ir-101" | "iso8859-2" | "iso88592" | "iso_8859-2" | "l2" | "latin2"
        | "csisolatin2" => "iso-8859-2",
        "windows-1252" | "ansi_x3.4-1968" | "ascii" | "cp1252" | "cp819" | "iso-8859-1"
        | "iso8859-1" | "latin1" | "us-ascii" | "windows-1251" => "windows-1252",
        _ => return None,
    })
}

fn lookup_u16(table: &[(u32, u16)], cp: u32) -> Option<u32> {
    table
        .binary_search_by(|&(c, _)| c.cmp(&cp))
        .ok()
        .map(|i| table[i].1 as u32)
}

/// Encode one code point in `enc`. `Ok(bytes)` on success; `Err(rep)` when unmappable, where `rep`
/// is the code point to use in the `&#rep;` numeric reference.
fn encode_cp(enc: &str, cp: u32) -> Result<Vec<u8>, u32> {
    match enc {
        "windows-1252" | "iso-8859-2" => {
            if cp < 0x80 {
                return Ok(vec![cp as u8]);
            }
            let table = if enc == "windows-1252" {
                WINDOWS_1252
            } else {
                ISO_8859_2
            };
            match table.binary_search_by(|&(c, _)| c.cmp(&cp)) {
                Ok(i) => Ok(vec![table[i].1]),
                Err(_) => Err(cp),
            }
        }
        "euc-kr" => {
            if cp < 0x80 {
                return Ok(vec![cp as u8]);
            }
            match lookup_u16(EUC_KR, cp) {
                Some(p) => Ok(vec![(p / 190 + 0x81) as u8, (p % 190 + 0x41) as u8]),
                None => Err(cp),
            }
        }
        "big5" => {
            if cp < 0x80 {
                return Ok(vec![cp as u8]);
            }
            match lookup_u16(BIG5, cp) {
                Some(p) => {
                    let lead = p / 157 + 0x81;
                    let mut trail = p % 157;
                    trail += if trail < 0x3F { 0x40 } else { 0x62 };
                    Ok(vec![lead as u8, trail as u8])
                }
                None => Err(cp),
            }
        }
        "shift_jis" => {
            if cp <= 0x80 {
                return Ok(vec![cp as u8]);
            }
            if cp == 0xA5 {
                return Ok(vec![0x5C]);
            }
            if cp == 0x203E {
                return Ok(vec![0x7E]);
            }
            if (0xFF61..=0xFF9F).contains(&cp) {
                return Ok(vec![(cp - 0xFF61 + 0xA1) as u8]);
            }
            let c = if cp == 0x2212 { 0xFF0D } else { cp };
            match lookup_u16(SHIFT_JIS, c) {
                Some(p) => {
                    let lead = p / 188;
                    let trail = p % 188;
                    let lead_byte = lead + if lead < 0x1F { 0x81 } else { 0xC1 };
                    let trail_byte = trail + if trail < 0x3F { 0x40 } else { 0x41 };
                    Ok(vec![lead_byte as u8, trail_byte as u8])
                }
                None => Err(cp),
            }
        }
        "gb18030" => {
            if cp < 0x80 {
                return Ok(vec![cp as u8]);
            }
            if cp == 0xE5E5 {
                return Err(cp);
            }
            if let Some(p) = lookup_u16(GB18030, cp) {
                let lead = p / 190 + 0x81;
                let mut trail = p % 190;
                trail += if trail < 0x3F { 0x40 } else { 0x41 };
                return Ok(vec![lead as u8, trail as u8]);
            }
            // Four-byte: find the range with the greatest start <= cp.
            let idx = GB18030_RANGES.partition_point(|&(start, _)| start <= cp);
            if idx == 0 {
                return Err(cp);
            }
            let (start, ptr_offset) = GB18030_RANGES[idx - 1];
            let mut p = ptr_offset + (cp - start);
            let b4 = p % 10 + 0x30;
            p /= 10;
            let b3 = p % 126 + 0x81;
            p /= 126;
            let b2 = p % 10 + 0x30;
            p /= 10;
            let b1 = p + 0x81;
            Ok(vec![b1 as u8, b2 as u8, b3 as u8, b4 as u8])
        }
        "iso-2022-jp" => unreachable!("iso-2022-jp is stateful; handled in percent_encode_query"),
        _ => Ok(vec![]),
    }
}

fn in_query_set_byte(b: u8, special: bool) -> bool {
    b <= 0x1F
        || b > 0x7E
        || matches!(b, b' ' | b'"' | b'#' | b'<' | b'>')
        || (special && b == b'\'')
}

fn push_byte(out: &mut String, b: u8, special: bool) {
    if in_query_set_byte(b, special) {
        out.push('%');
        out.push_str(&format!("{b:02X}"));
    } else {
        out.push(b as char);
    }
}

fn push_error(out: &mut String, rep: u32) {
    // The unmappable replacement is the pre-percent-encoded numeric reference `&#<decimal>;`
    // (`&`->%26, `#`->%23, `;`->%3B).
    out.push_str("%26%23");
    out.push_str(&rep.to_string());
    out.push_str("%3B");
}

/// Percent-encode `input` as a URL query using `enc` (a label() result) — i.e. WHATWG
/// "percent-encode after encoding" with the (special-)query percent-encode set.
pub fn percent_encode_query(enc: &str, input: &str, special: bool) -> String {
    let mut out = String::new();
    if enc == "iso-2022-jp" {
        iso2022jp_query(input, special, &mut out);
        return out;
    }
    for cp in input.chars() {
        match encode_cp(enc, cp as u32) {
            Ok(bytes) => {
                for b in bytes {
                    push_byte(&mut out, b, special);
                }
            }
            Err(rep) => push_error(&mut out, rep),
        }
    }
    out
}

#[derive(PartialEq)]
enum Jis {
    Ascii,
    Roman,
    Jis0208,
}

fn iso2022jp_query(input: &str, special: bool, out: &mut String) {
    let mut state = Jis::Ascii;
    let esc = |out: &mut String, bytes: &[u8], special: bool| {
        for &b in bytes {
            push_byte(out, b, special);
        }
    };
    for ch in input.chars() {
        let cp = ch as u32;
        if cp <= 0x7F {
            if matches!(cp, 0x0E | 0x0F | 0x1B) {
                push_error(out, 0xFFFD);
                continue;
            }
            if state != Jis::Ascii {
                // Leave Roman/JIS0208 for any ASCII code point (in Roman, 0x5C/0x7E differ).
                esc(out, &[0x1B, 0x28, 0x42], special);
                state = Jis::Ascii;
            }
            push_byte(out, cp as u8, special);
            continue;
        }
        if cp == 0xA5 || cp == 0x203E {
            if state != Jis::Roman {
                esc(out, &[0x1B, 0x28, 0x4A], special);
                state = Jis::Roman;
            }
            push_byte(out, if cp == 0xA5 { 0x5C } else { 0x7E }, special);
            continue;
        }
        if (0xFF61..=0xFF9F).contains(&cp) {
            push_error(out, cp);
            continue;
        }
        let c = if cp == 0x2212 { 0xFF0D } else { cp };
        match lookup_u16(JIS0208, c) {
            Some(p) => {
                if state != Jis::Jis0208 {
                    esc(out, &[0x1B, 0x24, 0x42], special);
                    state = Jis::Jis0208;
                }
                push_byte(out, (p / 94 + 0x21) as u8, special);
                push_byte(out, (p % 94 + 0x21) as u8, special);
            }
            None => push_error(out, cp),
        }
    }
    if state != Jis::Ascii {
        esc(out, &[0x1B, 0x28, 0x42], special);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encoding_matches_wpt() {
        // The percent-encoding.json cases (query column).
        let cases: &[(&str, &str, &str)] = &[
            ("big5", "\u{2020}", "%26%238224%3B"),
            ("euc-kr", "\u{2020}", "%A2%D3"),
            ("windows-1252", "\u{2020}", "%86"),
            ("iso-2022-jp", "\u{0e}A", "%26%2365533%3BA"),
            ("iso-2022-jp", "\u{203e}\\", "%1B(J~%1B(B\\"),
            ("gb18030", "\u{e5e5}", "%26%2358853%3B"),
            ("shift_jis", "\u{2212}", "%81|"),
            ("iso-8859-2", "\u{a2}", "%26%23162%3B"),
        ];
        for &(enc, input, expected) in cases {
            assert_eq!(
                percent_encode_query(enc, input, false),
                expected,
                "{enc} {input:?}"
            );
        }
    }
}
