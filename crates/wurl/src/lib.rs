//! A from-scratch implementation of the [WHATWG URL Standard](https://url.spec.whatwg.org/) parser,
//! host parser, serializer, and the URL-setter state overrides.
//!
//! We use this instead of the `url` crate because `url` (servo/rust-url 2.5.x) deviates from the
//! latest spec on a number of edges the WPT `url/` tests exercise — file-URL drive letters and
//! backslashes, non-special-scheme paths/hosts (including a panic on some host setters), the path
//! percent-encode set, and opaque-path trailing spaces. This module follows the spec's algorithms
//! directly, with no external URL/IDNA dependency: the host parser's domain-to-ASCII (a pragmatic
//! UTS-46 mapping + Punycode encode/decode, RFC 3492) is implemented here too.
//!
//! The public surface is small: [`Url::parse`], [`Url::parse_with_base`], component accessors used
//! by the JS URL record, and [`Url::set`] for the setters.

use std::fmt::Write as _;

mod idna;
mod unicode_tables;

// ---------------------------------------------------------------------------------------------
// Code-point classification + percent-encode sets
// ---------------------------------------------------------------------------------------------

fn is_ascii_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// The C0 control percent-encode set: C0 controls (<= U+001F) and every code point > U+007E.
fn in_c0_control_set(c: char) -> bool {
    c <= '\u{1f}' || c > '\u{7e}'
}
/// fragment set = C0 + U+0020, U+0022 ("), U+003C (<), U+003E (>), U+0060 (`)
fn in_fragment_set(c: char) -> bool {
    in_c0_control_set(c) || matches!(c, ' ' | '"' | '<' | '>' | '`')
}
/// query set = C0 + U+0020, U+0022, U+0023 (#), U+003C, U+003E
fn in_query_set(c: char) -> bool {
    in_c0_control_set(c) || matches!(c, ' ' | '"' | '#' | '<' | '>')
}
fn in_special_query_set(c: char) -> bool {
    in_query_set(c) || c == '\''
}
/// path set = query + U+003F (?), U+0060 (`), U+007B ({), U+007D (}) and U+005E (^)
fn in_path_set(c: char) -> bool {
    in_query_set(c) || matches!(c, '?' | '`' | '{' | '}' | '^')
}
/// userinfo set = path + / : ; = @ [ \ ] ^ |
fn in_userinfo_set(c: char) -> bool {
    in_path_set(c)
        || matches!(
            c,
            '/' | ':' | ';' | '=' | '@' | '[' | '\\' | ']' | '^' | '|'
        )
}

fn percent_encode_byte(out: &mut String, b: u8) {
    let _ = write!(out, "%{:02X}", b);
}

/// UTF-8 percent-encode `c` into `out` using the predicate `set` (encode when it returns true).
fn percent_encode_char(out: &mut String, c: char, set: fn(char) -> bool) {
    if set(c) {
        let mut buf = [0u8; 4];
        for b in c.encode_utf8(&mut buf).bytes() {
            percent_encode_byte(out, b);
        }
    } else {
        out.push(c);
    }
}

fn percent_encode_str(input: &str, set: fn(char) -> bool) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        percent_encode_char(&mut out, c, set);
    }
    out
}

/// Percent-decode a string to bytes.
fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && (bytes[i + 1] as char).is_ascii_hexdigit()
            && (bytes[i + 2] as char).is_ascii_hexdigit()
        {
            let h = (bytes[i + 1] as char).to_digit(16).unwrap() as u8;
            let l = (bytes[i + 2] as char).to_digit(16).unwrap() as u8;
            out.push(h * 16 + l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Schemes
// ---------------------------------------------------------------------------------------------

fn special_default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "ftp" => Some(21),
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}
fn is_special(scheme: &str) -> bool {
    matches!(scheme, "ftp" | "file" | "http" | "https" | "ws" | "wss")
}

// ---------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Host {
    Domain(String),
    Ipv4(u32),
    Ipv6([u16; 8]),
    Opaque(String),
    Empty,
}

impl Host {
    fn serialize(&self) -> String {
        match self {
            Host::Domain(d) => d.clone(),
            Host::Opaque(o) => o.clone(),
            Host::Empty => String::new(),
            Host::Ipv4(n) => {
                let mut n = *n;
                let mut parts = [0u32; 4];
                for i in (0..4).rev() {
                    parts[i] = n % 256;
                    n /= 256;
                }
                format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3])
            }
            Host::Ipv6(pieces) => format!("[{}]", serialize_ipv6(pieces)),
        }
    }
    fn is_empty(&self) -> bool {
        matches!(self, Host::Empty)
            || matches!(self, Host::Domain(d) if d.is_empty())
            || matches!(self, Host::Opaque(o) if o.is_empty())
    }
}

fn forbidden_host_code_point(c: char) -> bool {
    matches!(
        c,
        '\u{0}'
            | '\t'
            | '\n'
            | '\r'
            | ' '
            | '#'
            | '/'
            | ':'
            | '<'
            | '>'
            | '?'
            | '@'
            | '['
            | '\\'
            | ']'
            | '^'
            | '|'
    )
}
fn forbidden_domain_code_point(c: char) -> bool {
    forbidden_host_code_point(c) || c <= '\u{1f}' || c == '%' || c == '\u{7f}'
}

/// Host parser. `is_not_special` true → opaque-host parsing path.
fn parse_host(input: &str, is_not_special: bool) -> Result<Host, ()> {
    if input.starts_with('[') {
        if !input.ends_with(']') {
            return Err(());
        }
        let inner = &input[1..input.len() - 1];
        return Ok(Host::Ipv6(parse_ipv6(inner)?));
    }
    if is_not_special {
        return parse_opaque_host(input).map(Host::Opaque);
    }
    if input.is_empty() {
        return Err(());
    }
    // domain to ASCII (UTS-46) on the percent-decoded, UTF-8 domain.
    let decoded = percent_decode(input);
    let domain = String::from_utf8_lossy(&decoded);
    let ascii = idna::domain_to_ascii(&domain)?;
    if ascii.is_empty() {
        return Err(());
    }
    if ascii.chars().any(forbidden_domain_code_point) {
        return Err(());
    }
    if ends_in_a_number(&ascii) {
        return Ok(Host::Ipv4(parse_ipv4(&ascii)?));
    }
    Ok(Host::Domain(ascii))
}

fn parse_opaque_host(input: &str) -> Result<String, ()> {
    for c in input.chars() {
        if forbidden_host_code_point(c) && c != '%' {
            return Err(());
        }
    }
    Ok(percent_encode_str(input, in_c0_control_set))
}

/// A host string "ends in a number" if the last (dot-split) label is all-ASCII-digits, or is a
/// valid (hex/octal/decimal) number for IPv4 shorthand — the spec uses the simpler "last part is a
/// number" check.
fn ends_in_a_number(input: &str) -> bool {
    let parts: Vec<&str> = input.split('.').collect();
    let mut last = *parts.last().unwrap();
    if last.is_empty() {
        if parts.len() == 1 {
            return false;
        }
        last = parts[parts.len() - 2];
    }
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    parse_ipv4_number(last).is_some()
}

/// Parse an IPv4 "number" (the spec allows hex `0x`/octal `0`/decimal). Returns `None` if it isn't
/// syntactically a number; an overflowing value saturates to `u128::MAX` (still a number, but
/// `parse_ipv4`'s range check then rejects it).
fn parse_ipv4_number(input: &str) -> Option<u128> {
    if input.is_empty() {
        return None;
    }
    let mut s = input;
    let mut radix = 10u32;
    if s.len() >= 2 && (s.starts_with("0x") || s.starts_with("0X")) {
        s = &s[2..];
        radix = 16;
    } else if s.len() >= 2 && s.starts_with('0') {
        s = &s[1..];
        radix = 8;
    }
    if s.is_empty() {
        return Some(0);
    }
    if !s.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    Some(u128::from_str_radix(s, radix).unwrap_or(u128::MAX))
}

fn parse_ipv4(input: &str) -> Result<u32, ()> {
    let mut parts: Vec<&str> = input.split('.').collect();
    if let Some(last) = parts.last() {
        if last.is_empty() && parts.len() > 1 {
            parts.pop();
        }
    }
    if parts.len() > 4 {
        return Err(());
    }
    let mut numbers: Vec<u128> = Vec::new();
    for p in &parts {
        numbers.push(parse_ipv4_number(p).ok_or(())?);
    }
    for n in &numbers[..numbers.len().saturating_sub(1)] {
        if *n > 255 {
            return Err(());
        }
    }
    let last = *numbers.last().unwrap();
    if last >= 256u128.pow((5 - numbers.len()) as u32) {
        return Err(());
    }
    let mut ipv4: u128 = last;
    for (i, n) in numbers[..numbers.len() - 1].iter().enumerate() {
        ipv4 += *n * 256u128.pow((3 - i) as u32);
    }
    Ok(ipv4 as u32)
}

fn parse_ipv6(input: &str) -> Result<[u16; 8], ()> {
    let mut address = [0u16; 8];
    let mut piece_index = 0usize;
    let mut compress: Option<usize> = None;
    let chars: Vec<char> = input.chars().collect();
    let mut p = 0usize;
    let len = chars.len();
    if p < len && chars[p] == ':' {
        if p + 1 >= len || chars[p + 1] != ':' {
            return Err(());
        }
        p += 2;
        piece_index += 1;
        compress = Some(piece_index);
    }
    while p < len {
        if piece_index == 8 {
            return Err(());
        }
        if chars[p] == ':' {
            if compress.is_some() {
                return Err(());
            }
            p += 1;
            piece_index += 1;
            compress = Some(piece_index);
            continue;
        }
        let mut value: u16 = 0;
        let mut length = 0;
        while length < 4 && p < len && is_ascii_hex(chars[p]) {
            value = value * 16 + chars[p].to_digit(16).unwrap() as u16;
            p += 1;
            length += 1;
        }
        if p < len && chars[p] == '.' {
            if length == 0 {
                return Err(());
            }
            p -= length;
            if piece_index > 6 {
                return Err(());
            }
            let mut numbers_seen = 0;
            while p < len {
                let mut ipv4_piece: Option<u16> = None;
                if numbers_seen > 0 {
                    if chars[p] == '.' && numbers_seen < 4 {
                        p += 1;
                    } else {
                        return Err(());
                    }
                }
                if p >= len || !chars[p].is_ascii_digit() {
                    return Err(());
                }
                while p < len && chars[p].is_ascii_digit() {
                    let number = chars[p].to_digit(10).unwrap() as u16;
                    match ipv4_piece {
                        None => ipv4_piece = Some(number),
                        Some(0) => return Err(()),
                        Some(v) => ipv4_piece = Some(v * 10 + number),
                    }
                    if ipv4_piece.unwrap() > 255 {
                        return Err(());
                    }
                    p += 1;
                }
                address[piece_index] = address[piece_index] * 0x100 + ipv4_piece.unwrap();
                numbers_seen += 1;
                if numbers_seen == 2 || numbers_seen == 4 {
                    piece_index += 1;
                }
            }
            if numbers_seen != 4 {
                return Err(());
            }
            break;
        } else if p < len && chars[p] == ':' {
            p += 1;
            if p >= len {
                return Err(());
            }
        } else if p < len {
            return Err(());
        }
        address[piece_index] = value;
        piece_index += 1;
    }
    if let Some(c) = compress {
        let mut swaps = piece_index - c;
        let mut pi = 7;
        while pi != 0 && swaps > 0 {
            address.swap(pi, c + swaps - 1);
            pi -= 1;
            swaps -= 1;
        }
    } else if compress.is_none() && piece_index != 8 {
        return Err(());
    }
    Ok(address)
}

fn serialize_ipv6(pieces: &[u16; 8]) -> String {
    // Find the longest run of zero pieces (length > 1) to compress.
    let mut best_start = None;
    let mut best_len = 0;
    let mut cur_start = None;
    let mut cur_len = 0;
    for (i, &p) in pieces.iter().enumerate() {
        if p == 0 {
            if cur_start.is_none() {
                cur_start = Some(i);
                cur_len = 0;
            }
            cur_len += 1;
            if cur_len > best_len {
                best_len = cur_len;
                best_start = cur_start;
            }
        } else {
            cur_start = None;
            cur_len = 0;
        }
    }
    let compress = if best_len > 1 { best_start } else { None };
    let mut out = String::new();
    let mut i = 0;
    while i < 8 {
        if Some(i) == compress {
            out.push_str(if i == 0 { "::" } else { ":" });
            i += best_len;
            continue;
        }
        let _ = write!(out, "{:x}", pieces[i]);
        if i != 7 {
            out.push(':');
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// URL record
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum PathKind {
    Opaque(String),
    List(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Url {
    scheme: String,
    username: String,
    password: String,
    host: Option<Host>,
    port: Option<u16>,
    path: PathKind,
    query: Option<String>,
    fragment: Option<String>,
}

impl Url {
    fn new() -> Url {
        Url {
            scheme: String::new(),
            username: String::new(),
            password: String::new(),
            host: None,
            port: None,
            path: PathKind::List(Vec::new()),
            query: None,
            fragment: None,
        }
    }

    fn is_special(&self) -> bool {
        is_special(&self.scheme)
    }
    fn has_opaque_path(&self) -> bool {
        matches!(self.path, PathKind::Opaque(_))
    }
    fn includes_credentials(&self) -> bool {
        !self.username.is_empty() || !self.password.is_empty()
    }
    fn cannot_have_credentials(&self) -> bool {
        self.host_null_or_empty() || self.scheme == "file"
    }
    fn host_is_empty_none(&self) -> bool {
        self.host.is_none()
    }
    /// True if the URL has no host or an empty host (used by the credentials/port/scheme guards).
    fn host_null_or_empty(&self) -> bool {
        self.host.as_ref().is_none_or(Host::is_empty)
    }

    // --- public parse entry points ---

    /// Parse `input` as an absolute URL. `Err(())` (no detail — a spec parse failure carries none).
    #[allow(clippy::result_unit_err)]
    pub fn parse(input: &str) -> Result<Url, ()> {
        basic_parse(input, None, None)
    }
    /// Parse `input` against `base`. `Err(())` on failure (a spec parse failure carries no detail).
    #[allow(clippy::result_unit_err)]
    pub fn parse_with_base(input: &str, base: &Url) -> Result<Url, ()> {
        basic_parse(input, Some(base), None)
    }

    // --- component accessors (serialized forms used by the JS record) ---

    pub fn scheme(&self) -> &str {
        &self.scheme
    }
    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn password(&self) -> &str {
        &self.password
    }
    /// The explicit port, or the scheme's default port for a special scheme.
    pub fn port_or_default(&self) -> Option<u16> {
        self.port.or_else(|| special_default_port(&self.scheme))
    }
    pub fn hostname(&self) -> String {
        match &self.host {
            Some(h) => h.serialize(),
            None => String::new(),
        }
    }
    pub fn port_str(&self) -> String {
        self.port.map(|p| p.to_string()).unwrap_or_default()
    }
    pub fn host_str(&self) -> String {
        let h = self.hostname();
        match self.port {
            Some(p) => format!("{h}:{p}"),
            None => h,
        }
    }
    pub fn path_str(&self) -> String {
        match &self.path {
            // An opaque path that ends in spaces (only when a query/fragment follows) serializes its
            // final trailing space as %20 so it doesn't end in whitespace and round-trips.
            PathKind::Opaque(s) if s.ends_with(' ') => format!("{}%20", &s[..s.len() - 1]),
            PathKind::Opaque(s) => s.clone(),
            PathKind::List(segs) => {
                if segs.is_empty() {
                    String::new()
                } else {
                    let mut s = String::new();
                    for seg in segs {
                        s.push('/');
                        s.push_str(seg);
                    }
                    s
                }
            }
        }
    }
    pub fn query_str(&self) -> String {
        match &self.query {
            Some(q) if !q.is_empty() => format!("?{q}"),
            _ => String::new(),
        }
    }
    pub fn fragment_str(&self) -> String {
        match &self.fragment {
            Some(f) if !f.is_empty() => format!("#{f}"),
            _ => String::new(),
        }
    }

    pub fn href(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.scheme);
        out.push(':');
        if self.host.is_some() {
            out.push_str("//");
            if self.includes_credentials() {
                out.push_str(&self.username);
                if !self.password.is_empty() {
                    out.push(':');
                    out.push_str(&self.password);
                }
                out.push('@');
            }
            out.push_str(&self.hostname());
            if let Some(p) = self.port {
                out.push(':');
                out.push_str(&p.to_string());
            }
        } else if !self.has_opaque_path() && self.path_starts_with_double_slash() {
            // Per the serializer: if host is null, path is a list of >=2 with a leading empty
            // segment, prepend "/." so it doesn't read as an authority on reparse.
            out.push_str("/.");
        }
        out.push_str(&self.path_str());
        if let Some(q) = &self.query {
            out.push('?');
            out.push_str(q);
        }
        if let Some(f) = &self.fragment {
            out.push('#');
            out.push_str(f);
        }
        out
    }

    fn path_starts_with_double_slash(&self) -> bool {
        if let PathKind::List(segs) = &self.path {
            segs.len() >= 2 && segs[0].is_empty()
        } else {
            false
        }
    }

    pub fn origin(&self) -> String {
        match self.scheme.as_str() {
            "ftp" | "http" | "https" | "ws" | "wss" => {
                // tuple origin: scheme://host[:port]
                let mut s = String::new();
                s.push_str(&self.scheme);
                s.push_str("://");
                s.push_str(&self.hostname());
                if let Some(p) = self.port {
                    s.push(':');
                    s.push_str(&p.to_string());
                }
                s
            }
            "blob" => {
                // Parse the path as a URL; its origin is used only when its scheme is http/https/
                // file (the URL standard returns an opaque origin otherwise).
                if let PathKind::Opaque(p) = &self.path {
                    if let Ok(inner) = Url::parse(p) {
                        if matches!(inner.scheme.as_str(), "http" | "https" | "file") {
                            return inner.origin();
                        }
                    }
                }
                "null".to_string()
            }
            _ => "null".to_string(),
        }
    }

    pub fn cannot_be_a_base(&self) -> bool {
        self.has_opaque_path()
    }
}

/// Resolve `input` against the string `base` URL, returning the serialized href, or `None` if the
/// base or the resolved input fails to parse.
pub fn resolve(input: &str, base: &str) -> Option<String> {
    let b = Url::parse(base).ok()?;
    Url::parse_with_base(input, &b).ok().map(|u| u.href())
}

impl Url {
    // --- setters (state overrides) ---

    pub fn set(&mut self, prop: &str, value: &str) {
        match prop {
            "protocol" => self.set_scheme(value),
            "username" => {
                if !self.cannot_have_credentials() {
                    self.username = percent_encode_str(value, in_userinfo_set);
                }
            }
            "password" => {
                if !self.cannot_have_credentials() {
                    self.password = percent_encode_str(value, in_userinfo_set);
                }
            }
            "host" => {
                if !self.has_opaque_path() {
                    let _ =
                        basic_parse(value, None, Some((self.clone_into_override(), State::Host)))
                            .map(|u| *self = u);
                }
            }
            "hostname" => {
                if !self.has_opaque_path() {
                    let _ = basic_parse(
                        value,
                        None,
                        Some((self.clone_into_override(), State::Hostname)),
                    )
                    .map(|u| *self = u);
                }
            }
            "port" => {
                if !self.cannot_have_credentials() && self.scheme != "file" {
                    if value.is_empty() {
                        self.port = None;
                    } else {
                        let _ = basic_parse(
                            value,
                            None,
                            Some((self.clone_into_override(), State::Port)),
                        )
                        .map(|u| *self = u);
                    }
                }
            }
            "pathname" => {
                if !self.has_opaque_path() {
                    self.path = PathKind::List(Vec::new());
                    let _ = basic_parse(
                        value,
                        None,
                        Some((self.clone_into_override(), State::PathStart)),
                    )
                    .map(|u| *self = u);
                }
            }
            "search" => {
                if value.is_empty() {
                    self.query = None;
                } else {
                    let v = value.strip_prefix('?').unwrap_or(value);
                    self.query = Some(String::new());
                    let _ = basic_parse(v, None, Some((self.clone_into_override(), State::Query)))
                        .map(|u| *self = u);
                }
            }
            "hash" => {
                if value.is_empty() {
                    self.fragment = None;
                } else {
                    let v = value.strip_prefix('#').unwrap_or(value);
                    self.fragment = Some(String::new());
                    let _ =
                        basic_parse(v, None, Some((self.clone_into_override(), State::Fragment)))
                            .map(|u| *self = u);
                }
            }
            "href" => {
                // The href setter is a full reparse; an invalid value leaves the URL unchanged here
                // (the JS layer throws a TypeError on the null result).
                if let Ok(u) = Url::parse(value) {
                    *self = u;
                }
            }
            _ => {}
        }
    }

    fn clone_into_override(&self) -> Url {
        self.clone()
    }

    fn set_scheme(&mut self, value: &str) {
        // The scheme setter parses "value:" with scheme-start state override.
        let mut input = value.to_string();
        input.push(':');
        let _ =
            basic_parse(&input, None, Some((self.clone(), State::SchemeStart))).map(|u| *self = u);
    }
}

// ---------------------------------------------------------------------------------------------
// The basic URL parser
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    SchemeStart,
    Scheme,
    NoScheme,
    SpecialRelativeOrAuthority,
    PathOrAuthority,
    Relative,
    RelativeSlash,
    SpecialAuthoritySlashes,
    SpecialAuthorityIgnoreSlashes,
    Authority,
    Host,
    Hostname,
    Port,
    File,
    FileSlash,
    FileHost,
    PathStart,
    Path,
    OpaquePath,
    Query,
    Fragment,
}

fn is_windows_drive_letter(s: &[char]) -> bool {
    s.len() == 2 && s[0].is_ascii_alphabetic() && (s[1] == ':' || s[1] == '|')
}
fn is_normalized_windows_drive_letter(s: &[char]) -> bool {
    s.len() == 2 && s[0].is_ascii_alphabetic() && s[1] == ':'
}
fn starts_with_windows_drive_letter(s: &[char]) -> bool {
    s.len() >= 2
        && s[0].is_ascii_alphabetic()
        && (s[1] == ':' || s[1] == '|')
        && (s.len() == 2 || matches!(s[2], '/' | '\\' | '?' | '#'))
}

fn is_single_dot(seg: &str) -> bool {
    seg == "." || seg.eq_ignore_ascii_case("%2e")
}
fn is_double_dot(seg: &str) -> bool {
    let l = seg.to_ascii_lowercase();
    l == ".." || l == ".%2e" || l == "%2e." || l == "%2e%2e"
}

fn shorten_path(url: &mut Url) {
    if let PathKind::List(segs) = &mut url.path {
        if url.scheme == "file"
            && segs.len() == 1
            && is_normalized_windows_drive_letter(&segs[0].chars().collect::<Vec<_>>())
        {
            return;
        }
        segs.pop();
    }
}

#[allow(clippy::too_many_lines)]
fn basic_parse(
    input_raw: &str,
    base: Option<&Url>,
    state_override: Option<(Url, State)>,
) -> Result<Url, ()> {
    let has_override = state_override.is_some();
    let (mut url, mut state) = match &state_override {
        Some((u, s)) => (u.clone(), *s),
        None => (Url::new(), State::SchemeStart),
    };

    // Remove leading/trailing C0 control or space (only when not a state override).
    let mut input = input_raw;
    if !has_override {
        input = input.trim_matches(|c: char| c <= ' ');
    }
    // Remove all ASCII tab or newline.
    let cleaned: String = input
        .chars()
        .filter(|&c| c != '\t' && c != '\n' && c != '\r')
        .collect();
    let chars: Vec<char> = cleaned.chars().collect();
    let len = chars.len();

    let mut buffer = String::new();
    let mut at_sign_seen = false;
    let mut inside_brackets = false;
    let mut password_token_seen = false;
    let mut i = 0usize;

    // We iterate with an index that can equal `len` (the EOF position).
    loop {
        let c = if i < len { Some(chars[i]) } else { None };
        match state {
            State::SchemeStart => {
                if let Some(ch) = c {
                    if ch.is_ascii_alphabetic() {
                        buffer.push(ch.to_ascii_lowercase());
                        state = State::Scheme;
                    } else if !has_override {
                        state = State::NoScheme;
                        continue; // reprocess without incrementing
                    } else {
                        return Err(());
                    }
                } else if !has_override {
                    state = State::NoScheme;
                    continue;
                } else {
                    return Err(());
                }
            }
            State::Scheme => {
                if let Some(ch) = c {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.') {
                        buffer.push(ch.to_ascii_lowercase());
                    } else if ch == ':' {
                        if has_override {
                            let cur_special = url.is_special();
                            let new_special = is_special(&buffer);
                            if cur_special != new_special {
                                return Ok(url);
                            }
                            if (url.includes_credentials() || url.port.is_some())
                                && buffer == "file"
                            {
                                return Ok(url);
                            }
                            if url.scheme == "file" && url.host_null_or_empty() {
                                return Ok(url);
                            }
                        }
                        url.scheme = std::mem::take(&mut buffer);
                        if has_override {
                            if url.port == special_default_port(&url.scheme) {
                                url.port = None;
                            }
                            return Ok(url);
                        }
                        let remaining = &chars[i + 1..];
                        if url.scheme == "file" {
                            state = State::File;
                            // The spec checks "//" but proceeds either way.
                            i += 1;
                            continue;
                        } else if url.is_special()
                            && base.map(|b| b.scheme == url.scheme).unwrap_or(false)
                        {
                            state = State::SpecialRelativeOrAuthority;
                        } else if url.is_special() {
                            state = State::SpecialAuthoritySlashes;
                        } else if remaining.first() == Some(&'/') {
                            state = State::PathOrAuthority;
                            i += 1; // consume the '/'
                            i += 1; // consume the ':'
                            continue;
                        } else {
                            url.path = PathKind::Opaque(String::new());
                            state = State::OpaquePath;
                        }
                    } else if !has_override {
                        buffer.clear();
                        state = State::NoScheme;
                        i = 0;
                        continue;
                    } else {
                        return Err(());
                    }
                } else if !has_override {
                    buffer.clear();
                    state = State::NoScheme;
                    i = 0;
                    continue;
                } else {
                    return Err(());
                }
            }
            State::NoScheme => {
                let base = base.ok_or(())?;
                if base.has_opaque_path() {
                    if c == Some('#') {
                        url.scheme = base.scheme.clone();
                        url.path = base.path.clone();
                        url.query = base.query.clone();
                        url.fragment = Some(String::new());
                        state = State::Fragment;
                    } else {
                        return Err(());
                    }
                } else if base.scheme != "file" {
                    state = State::Relative;
                    continue;
                } else {
                    state = State::File;
                    continue;
                }
            }
            State::SpecialRelativeOrAuthority => {
                if c == Some('/') && chars.get(i + 1) == Some(&'/') {
                    state = State::SpecialAuthorityIgnoreSlashes;
                    i += 1;
                } else {
                    state = State::Relative;
                    continue;
                }
            }
            State::PathOrAuthority => {
                if c == Some('/') {
                    state = State::Authority;
                } else {
                    state = State::Path;
                    continue;
                }
            }
            State::Relative => {
                let base = base.ok_or(())?;
                url.scheme = base.scheme.clone();
                match c {
                    Some('/') => state = State::RelativeSlash,
                    Some('\\') if url.is_special() => state = State::RelativeSlash,
                    _ => {
                        url.username = base.username.clone();
                        url.password = base.password.clone();
                        url.host = base.host.clone();
                        url.port = base.port;
                        url.path = base.path.clone();
                        url.query = base.query.clone();
                        match c {
                            Some('?') => {
                                url.query = Some(String::new());
                                state = State::Query;
                            }
                            Some('#') => {
                                url.fragment = Some(String::new());
                                state = State::Fragment;
                            }
                            Some(_) => {
                                url.query = None;
                                shorten_path(&mut url);
                                state = State::Path;
                                continue;
                            }
                            None => {}
                        }
                    }
                }
            }
            State::RelativeSlash => {
                let base = base.ok_or(())?;
                if url.is_special() && (c == Some('/') || c == Some('\\')) {
                    state = State::SpecialAuthorityIgnoreSlashes;
                } else if c == Some('/') {
                    state = State::Authority;
                } else {
                    url.username = base.username.clone();
                    url.password = base.password.clone();
                    url.host = base.host.clone();
                    url.port = base.port;
                    state = State::Path;
                    continue;
                }
            }
            State::SpecialAuthoritySlashes => {
                if c == Some('/') && chars.get(i + 1) == Some(&'/') {
                    state = State::SpecialAuthorityIgnoreSlashes;
                    i += 1;
                } else {
                    state = State::SpecialAuthorityIgnoreSlashes;
                    continue;
                }
            }
            State::SpecialAuthorityIgnoreSlashes => {
                if c != Some('/') && c != Some('\\') {
                    state = State::Authority;
                    continue;
                }
            }
            State::Authority => {
                match c {
                    Some('@') => {
                        if at_sign_seen {
                            buffer.insert_str(0, "%40");
                        }
                        at_sign_seen = true;
                        for ch in buffer.chars() {
                            if ch == ':' && !password_token_seen {
                                password_token_seen = true;
                                continue;
                            }
                            let enc = percent_encode_str(&ch.to_string(), in_userinfo_set);
                            if password_token_seen {
                                url.password.push_str(&enc);
                            } else {
                                url.username.push_str(&enc);
                            }
                        }
                        buffer.clear();
                    }
                    None | Some('/') | Some('?') | Some('#') => {
                        if at_sign_seen && buffer.is_empty() {
                            return Err(());
                        }
                        // back up to the start of buffer
                        i -= buffer.chars().count();
                        buffer.clear();
                        state = State::Host;
                        continue;
                    }
                    Some('\\') if url.is_special() => {
                        if at_sign_seen && buffer.is_empty() {
                            return Err(());
                        }
                        i -= buffer.chars().count();
                        buffer.clear();
                        state = State::Host;
                        continue;
                    }
                    Some(ch) => buffer.push(ch),
                }
            }
            State::Host | State::Hostname => {
                if has_override && url.scheme == "file" {
                    state = State::FileHost;
                    continue;
                } else if c == Some(':') && !inside_brackets {
                    if buffer.is_empty() {
                        return Err(());
                    }
                    if has_override && state == State::Hostname {
                        return Ok(url);
                    }
                    let host = parse_host(&buffer, !url.is_special())?;
                    url.host = Some(host);
                    buffer.clear();
                    state = State::Port;
                } else if matches!(c, None | Some('/') | Some('?') | Some('#'))
                    || (url.is_special() && c == Some('\\'))
                {
                    if url.is_special() && buffer.is_empty() {
                        return Err(());
                    }
                    if has_override
                        && buffer.is_empty()
                        && (url.includes_credentials() || url.port.is_some())
                    {
                        return Ok(url);
                    }
                    let host = parse_host(&buffer, !url.is_special())?;
                    url.host = Some(host);
                    buffer.clear();
                    state = State::PathStart;
                    if has_override {
                        return Ok(url);
                    }
                    continue;
                } else {
                    if c == Some('[') {
                        inside_brackets = true;
                    } else if c == Some(']') {
                        inside_brackets = false;
                    }
                    buffer.push(c.unwrap());
                }
            }
            State::Port => {
                if let Some(ch) = c {
                    if ch.is_ascii_digit() {
                        buffer.push(ch);
                    } else if matches!(ch, '/' | '?' | '#')
                        || (url.is_special() && ch == '\\')
                        || has_override
                    {
                        if !buffer.is_empty() {
                            match buffer.parse::<u32>() {
                                Ok(num) if num <= 65535 => {
                                    let port = num as u16;
                                    url.port = if Some(port) == special_default_port(&url.scheme) {
                                        None
                                    } else {
                                        Some(port)
                                    };
                                }
                                // A port-parse failure leaves any already-applied host in place when
                                // this is a setter (state override); otherwise the whole parse fails.
                                _ => return if has_override { Ok(url) } else { Err(()) },
                            }
                            buffer.clear();
                        }
                        if has_override {
                            return Ok(url);
                        }
                        state = State::PathStart;
                        continue;
                    } else {
                        return Err(());
                    }
                } else {
                    if !buffer.is_empty() {
                        match buffer.parse::<u32>() {
                            Ok(num) if num <= 65535 => {
                                let port = num as u16;
                                url.port = if Some(port) == special_default_port(&url.scheme) {
                                    None
                                } else {
                                    Some(port)
                                };
                            }
                            _ => return if has_override { Ok(url) } else { Err(()) },
                        }
                        buffer.clear();
                    }
                    if has_override {
                        return Ok(url);
                    }
                    state = State::PathStart;
                    continue;
                }
            }
            State::File => {
                url.scheme = "file".to_string();
                url.host = Some(Host::Empty);
                if c == Some('/') || c == Some('\\') {
                    state = State::FileSlash;
                } else if let Some(base) = base.filter(|b| b.scheme == "file") {
                    url.host = base.host.clone();
                    url.path = base.path.clone();
                    url.query = base.query.clone();
                    match c {
                        Some('?') => {
                            url.query = Some(String::new());
                            state = State::Query;
                        }
                        Some('#') => {
                            url.fragment = Some(String::new());
                            state = State::Fragment;
                        }
                        Some(_) => {
                            url.query = None;
                            if !starts_with_windows_drive_letter(&chars[i..]) {
                                shorten_path(&mut url);
                            } else {
                                url.path = PathKind::List(Vec::new());
                            }
                            state = State::Path;
                            continue;
                        }
                        None => {}
                    }
                } else {
                    state = State::Path;
                    continue;
                }
            }
            State::FileSlash => {
                if c == Some('/') || c == Some('\\') {
                    state = State::FileHost;
                } else {
                    if let Some(base) = base.filter(|b| b.scheme == "file") {
                        url.host = base.host.clone();
                        if !starts_with_windows_drive_letter(&chars[i..])
                            && file_base_has_drive_letter(base)
                        {
                            if let PathKind::List(segs) = &base.path {
                                if let Some(first) = segs.first() {
                                    url.path = PathKind::List(vec![first.clone()]);
                                }
                            }
                        }
                    }
                    state = State::Path;
                    continue;
                }
            }
            State::FileHost => {
                if matches!(c, None | Some('/') | Some('\\') | Some('?') | Some('#')) {
                    if !has_override && is_windows_drive_letter(&buffer.chars().collect::<Vec<_>>())
                    {
                        state = State::Path;
                        continue;
                    } else if buffer.is_empty() {
                        url.host = Some(Host::Empty);
                        if has_override {
                            return Ok(url);
                        }
                        state = State::PathStart;
                        continue;
                    } else {
                        let mut host = parse_host(&buffer, !url.is_special())?;
                        if host == Host::Domain("localhost".to_string()) {
                            host = Host::Empty;
                        }
                        url.host = Some(host);
                        if has_override {
                            return Ok(url);
                        }
                        buffer.clear();
                        state = State::PathStart;
                        continue;
                    }
                } else {
                    buffer.push(c.unwrap());
                }
            }
            State::PathStart => {
                if url.is_special() {
                    state = State::Path;
                    if c != Some('/') && c != Some('\\') {
                        continue;
                    }
                } else if !has_override && c == Some('?') {
                    url.query = Some(String::new());
                    state = State::Query;
                } else if !has_override && c == Some('#') {
                    url.fragment = Some(String::new());
                    state = State::Fragment;
                } else if c.is_some() {
                    state = State::Path;
                    if c != Some('/') {
                        continue;
                    }
                } else if has_override && url.host_is_empty_none() {
                    if let PathKind::List(segs) = &mut url.path {
                        segs.push(String::new());
                    }
                }
            }
            State::Path => {
                let end_segment = matches!(c, None | Some('/'))
                    || (url.is_special() && c == Some('\\'))
                    || (!has_override && matches!(c, Some('?') | Some('#')));
                if end_segment {
                    if is_double_dot(&buffer) {
                        shorten_path(&mut url);
                        if c != Some('/') && !(url.is_special() && c == Some('\\')) {
                            if let PathKind::List(segs) = &mut url.path {
                                segs.push(String::new());
                            }
                        }
                    } else if is_single_dot(&buffer) {
                        if c != Some('/') && !(url.is_special() && c == Some('\\')) {
                            if let PathKind::List(segs) = &mut url.path {
                                segs.push(String::new());
                            }
                        }
                    } else {
                        if url.scheme == "file"
                            && url.path_is_empty_list()
                            && is_windows_drive_letter(&buffer.chars().collect::<Vec<_>>())
                        {
                            let b: Vec<char> = buffer.chars().collect();
                            buffer = format!("{}:", b[0]);
                        }
                        if let PathKind::List(segs) = &mut url.path {
                            segs.push(std::mem::take(&mut buffer));
                        }
                    }
                    buffer.clear();
                    match c {
                        Some('?') => {
                            url.query = Some(String::new());
                            state = State::Query;
                        }
                        Some('#') => {
                            url.fragment = Some(String::new());
                            state = State::Fragment;
                        }
                        _ => {}
                    }
                } else {
                    let ch = c.unwrap();
                    percent_encode_char(&mut buffer, ch, in_path_set);
                }
            }
            State::OpaquePath => match c {
                Some('?') => {
                    url.query = Some(String::new());
                    state = State::Query;
                }
                Some('#') => {
                    url.fragment = Some(String::new());
                    state = State::Fragment;
                }
                Some(ch) => {
                    if let PathKind::Opaque(p) = &mut url.path {
                        percent_encode_char(p, ch, in_c0_control_set);
                    }
                }
                None => {}
            },
            State::Query => {
                let special = url.is_special();
                if !has_override && c == Some('#') {
                    url.fragment = Some(String::new());
                    state = State::Fragment;
                } else if let Some(ch) = c {
                    let set = if special {
                        in_special_query_set
                    } else {
                        in_query_set
                    };
                    if let Some(q) = &mut url.query {
                        percent_encode_char(q, ch, set);
                    } else {
                        let mut q = String::new();
                        percent_encode_char(&mut q, ch, set);
                        url.query = Some(q);
                    }
                }
            }
            State::Fragment => {
                if let Some(ch) = c {
                    if let Some(f) = &mut url.fragment {
                        percent_encode_char(f, ch, in_fragment_set);
                    } else {
                        let mut f = String::new();
                        percent_encode_char(&mut f, ch, in_fragment_set);
                        url.fragment = Some(f);
                    }
                }
            }
        }
        if i >= len {
            break;
        }
        i += 1;
    }
    Ok(url)
}

fn file_base_has_drive_letter(base: &Url) -> bool {
    if let PathKind::List(segs) = &base.path {
        if let Some(first) = segs.first() {
            return is_normalized_windows_drive_letter(&first.chars().collect::<Vec<_>>());
        }
    }
    false
}

impl Url {
    fn path_is_empty_list(&self) -> bool {
        matches!(&self.path, PathKind::List(s) if s.is_empty())
    }
}

#[cfg(test)]
mod conformance {
    use super::*;

    /// Replace lone-surrogate `\uXXXX` escapes (which serde_json can't parse) with `�`,
    /// matching the USVString coercion the engine applies before URL parsing.
    fn sanitize_lone_surrogates(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        let hex = |p: usize| -> Option<u32> {
            if p + 6 <= b.len() && b[p] == b'\\' && b[p + 1] == b'u' {
                std::str::from_utf8(&b[p + 2..p + 6])
                    .ok()
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
            } else {
                None
            }
        };
        while i < b.len() {
            if let Some(u) = hex(i) {
                if (0xd800..0xdc00).contains(&u) {
                    // high surrogate: keep only if a low surrogate follows
                    if matches!(hex(i + 6), Some(l) if (0xdc00..0xe000).contains(&l)) {
                        out.push_str(&s[i..i + 12]);
                        i += 12;
                        continue;
                    }
                    out.push_str("\\ufffd");
                    i += 6;
                    continue;
                } else if (0xdc00..0xe000).contains(&u) {
                    out.push_str("\\ufffd");
                    i += 6;
                    continue;
                }
            }
            out.push(b[i] as char);
            i += 1;
        }
        out
    }

    #[test]
    fn idna_testv2_conformance() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../wpt/url/resources/IdnaTestV2.json"
        ))
        .unwrap();
        // serde_json rejects lone-surrogate \uXXXX escapes; in the engine these are USVString-coerced
        // to U+FFFD before the host parser, so rewrite lone surrogates to � for parsing.
        let data = sanitize_lone_surrogates(&raw);
        let cases: serde_json::Value = serde_json::from_str(&data).unwrap();
        let (mut pass, mut fail, mut examples) = (0u32, 0u32, Vec::new());
        for case in cases.as_array().unwrap() {
            if !case.is_object() {
                continue;
            }
            let input = case["input"].as_str().unwrap_or("");
            let expected = case["output"].as_str().unwrap_or("");
            let got = crate::idna::domain_to_ascii(input);
            let ok = if expected.is_empty() {
                got.is_err()
            } else {
                got.as_deref() == Ok(expected)
            };
            if ok {
                pass += 1;
            } else {
                fail += 1;
                if examples.len() < 30 {
                    examples.push(format!("{input:?} exp {expected:?} got {got:?}"));
                }
            }
        }
        eprintln!("IdnaTestV2: {pass} pass / {fail} fail");
        for e in &examples {
            eprintln!("  FAIL {e}");
        }
    }

    fn record(u: &Url) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("href".into(), u.href());
        m.insert("protocol".into(), format!("{}:", u.scheme()));
        m.insert("username".into(), u.username().to_string());
        m.insert("password".into(), u.password().to_string());
        m.insert("host".into(), u.host_str());
        m.insert("hostname".into(), u.hostname());
        m.insert("port".into(), u.port_str());
        m.insert("pathname".into(), u.path_str());
        m.insert("search".into(), u.query_str());
        m.insert("hash".into(), u.fragment_str());
        m
    }

    #[test]
    fn urltestdata_conformance() {
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../wpt/url/resources/urltestdata.json"
        ))
        .unwrap();
        let cases: serde_json::Value = serde_json::from_str(&data).unwrap();
        let fields = [
            "href", "protocol", "username", "password", "host", "hostname", "port", "pathname",
            "search", "hash",
        ];
        let (mut pass, mut fail, mut fail_examples) = (0u32, 0u32, Vec::new());
        for case in cases.as_array().unwrap() {
            if !case.is_object() {
                continue;
            }
            let input = case["input"].as_str().unwrap_or("");
            let base = case.get("base").and_then(|b| b.as_str());
            let parsed = match base {
                Some(b) => match Url::parse(b) {
                    Ok(bu) => Url::parse_with_base(input, &bu),
                    Err(_) => Err(()),
                },
                None => Url::parse(input),
            };
            let failure = case
                .get("failure")
                .and_then(|f| f.as_bool())
                .unwrap_or(false);
            if failure {
                match parsed {
                    Err(_) => pass += 1,
                    Ok(u) => {
                        fail += 1;
                        if fail_examples.len() < 25 {
                            fail_examples.push(format!(
                                "[should fail] {input:?} base={base:?} -> {:?}",
                                u.href()
                            ));
                        }
                    }
                }
                continue;
            }
            let u = match parsed {
                Ok(u) => u,
                Err(_) => {
                    fail += 1;
                    if fail_examples.len() < 25 {
                        fail_examples.push(format!("[parse err] {input:?} base={base:?}"));
                    }
                    continue;
                }
            };
            let rec = record(&u);
            let mut mismatches = Vec::new();
            for f in fields {
                if let Some(exp) = case.get(f).and_then(|v| v.as_str()) {
                    let got = rec.get(f).map(String::as_str).unwrap_or("");
                    if got != exp {
                        mismatches.push(format!("{f}: exp {exp:?} got {got:?}"));
                    }
                }
            }
            if mismatches.is_empty() {
                pass += 1;
            } else {
                fail += 1;
                if fail_examples.len() < 25 {
                    fail_examples.push(format!(
                        "{input:?} base={base:?} | {}",
                        mismatches.join("; ")
                    ));
                }
            }
        }
        eprintln!("urltestdata: {pass} pass / {fail} fail");
        for e in &fail_examples {
            eprintln!("  FAIL {e}");
        }
    }
}

#[cfg(test)]
mod setters_conformance {
    use super::*;

    #[test]
    fn setters_tests_conformance() {
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../wpt/url/resources/setters_tests.json"
        ))
        .unwrap();
        let all: serde_json::Value = serde_json::from_str(&data).unwrap();
        let (mut pass, mut fail, mut examples) = (0u32, 0u32, Vec::new());
        for (setter, cases) in all.as_object().unwrap() {
            if setter == "comment" {
                continue;
            }
            for case in cases.as_array().unwrap() {
                let href = case["href"].as_str().unwrap();
                let new_value = case["new_value"].as_str().unwrap();
                let expected = &case["expected"];
                let mut u = match Url::parse(href) {
                    Ok(u) => u,
                    Err(_) => {
                        fail += 1;
                        continue;
                    }
                };
                u.set(setter, new_value);
                let getters = [
                    ("href", u.href()),
                    ("protocol", format!("{}:", u.scheme())),
                    ("username", u.username().to_string()),
                    ("password", u.password().to_string()),
                    ("host", u.host_str()),
                    ("hostname", u.hostname()),
                    ("port", u.port_str()),
                    ("pathname", u.path_str()),
                    ("search", u.query_str()),
                    ("hash", u.fragment_str()),
                ];
                let mut mism = Vec::new();
                for (k, got) in &getters {
                    if let Some(exp) = expected.get(k).and_then(|v| v.as_str()) {
                        if got != exp {
                            mism.push(format!("{k}: exp {exp:?} got {got:?}"));
                        }
                    }
                }
                if mism.is_empty() {
                    pass += 1;
                } else {
                    fail += 1;
                    if examples.len() < 30 {
                        examples.push(format!(
                            "[{setter}] <{href}>.{setter}={new_value:?} | {}",
                            mism.join("; ")
                        ));
                    }
                }
            }
        }
        eprintln!("setters: {pass} pass / {fail} fail");
        for e in &examples {
            eprintln!("  FAIL {e}");
        }
    }
}
