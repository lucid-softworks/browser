use crate::*;

/// Parse a color to opaque `(r, g, b)`. Supports hex (`#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`),
/// named colors, and the functional forms `rgb()`/`rgba()`, `hsl()`/`hsla()`, `oklch()`, and
/// `oklab()`. Alpha is parsed but dropped (treated as opaque). Returns `None` if unrecognized.
///
/// `current` supplies the value of `currentColor`; `inherited` supplies the value used for
/// `inherit`. Keywords `transparent`/`initial` return `None` (caller treats as "no change /
/// no color").
pub(crate) fn parse_color_ctx(
    val: &str,
    current: (u8, u8, u8),
    inherited: (u8, u8, u8),
) -> Option<(u8, u8, u8)> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();

    // Keywords.
    match lower.as_str() {
        "currentcolor" => return Some(current),
        "inherit" => return Some(inherited),
        "transparent" | "initial" | "unset" | "none" | "revert" => return None,
        _ => {}
    }

    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex(hex);
    }

    // Functional color: name( args ).
    if let Some(open) = v.find('(') {
        if v.ends_with(')') {
            let func = v[..open].trim().to_ascii_lowercase();
            let inner = &v[open + 1..v.len() - 1];
            return parse_color_function(&func, inner);
        }
    }

    parse_named_color(&lower)
}

/// Convenience wrapper used where no element context is needed (currentColor/inherit map to a
/// neutral default). Prefer [`parse_color_ctx`] in the cascade.
#[cfg(test)]
pub(crate) fn parse_color(val: &str) -> Option<(u8, u8, u8)> {
    parse_color_ctx(val, (0, 0, 0), (0, 0, 0))
}

/// Parse a color into [`Rgba`], preserving alpha (unlike [`parse_color_ctx`] which drops it).
/// Handles `transparent` (→ alpha 0), `#rgba`/`#rrggbbaa` hex alpha, and the `/ alpha` or
/// 4th-component alpha of `rgba()`/`hsla()`. `currentColor` resolves to `current` (opaque).
/// Used by gradients and box-shadows where alpha matters. Returns `None` if unrecognized.
pub(crate) fn parse_rgba_ctx(
    val: &str,
    current: (u8, u8, u8),
    inherited: (u8, u8, u8),
) -> Option<Rgba> {
    let v = val.trim();
    let lower = v.to_ascii_lowercase();
    match lower.as_str() {
        "currentcolor" => {
            return Some(Rgba {
                r: current.0,
                g: current.1,
                b: current.2,
                a: 255,
            })
        }
        "inherit" => {
            return Some(Rgba {
                r: inherited.0,
                g: inherited.1,
                b: inherited.2,
                a: 255,
            })
        }
        "transparent" => {
            return Some(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            })
        }
        "initial" | "unset" | "none" | "revert" => return None,
        _ => {}
    }
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex_alpha(hex);
    }
    // Functional form: extract alpha from the args, then defer the rgb to parse_color_function.
    if let Some(open) = v.find('(') {
        if v.ends_with(')') {
            let func = v[..open].trim().to_ascii_lowercase();
            let inner = &v[open + 1..v.len() - 1];
            let (r, g, b) = parse_color_function(&func, inner)?;
            let alpha = parse_func_alpha(inner).unwrap_or(255);
            return Some(Rgba { r, g, b, a: alpha });
        }
    }
    parse_named_color(&lower).map(|(r, g, b)| Rgba { r, g, b, a: 255 })
}

/// Extract the alpha byte from a functional color's argument body (between the parens). Looks for
/// either a `/ <alpha>` segment or a 4th comma/space-separated component. `None` if no alpha.
pub(crate) fn parse_func_alpha(inner: &str) -> Option<u8> {
    // `/ alpha` form (modern syntax).
    if let Some(slash) = inner.split('/').nth(1) {
        return alpha_to_u8(slash.trim());
    }
    // Legacy 4-component form: rgba(r,g,b,a) / hsla(h,s,l,a).
    let toks: Vec<&str> = inner
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() >= 4 {
        return alpha_to_u8(toks[3]);
    }
    None
}

/// Parse an alpha token (`0.5`, `50%`, `1`) into a 0..=255 byte.
pub(crate) fn alpha_to_u8(tok: &str) -> Option<u8> {
    let t = tok.trim();
    let f = if let Some(p) = t.strip_suffix('%') {
        p.trim().parse::<f32>().ok()? / 100.0
    } else {
        t.parse::<f32>().ok()?
    };
    Some((f.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Parse a hex color preserving alpha (`#rgba`/`#rrggbbaa` carry alpha; `#rgb`/`#rrggbb` → opaque).
pub(crate) fn parse_hex_alpha(hex: &str) -> Option<Rgba> {
    let h = hex.trim();
    let hx = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        3 => {
            let (r, g, b) = parse_hex(h)?;
            Some(Rgba { r, g, b, a: 255 })
        }
        4 => {
            let (r, g, b) = parse_hex(&h[0..3])?;
            let a = hx(&h[3..4])?;
            Some(Rgba { r, g, b, a: a * 17 })
        }
        6 => {
            let (r, g, b) = parse_hex(h)?;
            Some(Rgba { r, g, b, a: 255 })
        }
        8 => {
            let (r, g, b) = parse_hex(&h[0..6])?;
            let a = hx(&h[6..8])?;
            Some(Rgba { r, g, b, a })
        }
        _ => None,
    }
}

/// Parse a functional color body (the text between the parens), given the lowercased function
/// name. Handles `rgb`/`rgba`/`hsl`/`hsla`/`oklch`/`oklab`.
pub(crate) fn parse_color_function(func: &str, inner: &str) -> Option<(u8, u8, u8)> {
    // Relative-color syntax (`rgb(from red r g b)`) and other exotic forms are not supported;
    // bail out so the caller can fall back rather than mis-parse.
    if inner.trim_start().to_ascii_lowercase().starts_with("from ") {
        return None;
    }
    // Split on commas and/or whitespace; also strip an optional `/ alpha` segment.
    let main = inner.split('/').next().unwrap_or(inner);
    let toks: Vec<&str> = main
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    match func {
        "rgb" | "rgba" => {
            if toks.len() < 3 {
                return None;
            }
            Some((
                parse_rgb_component(toks[0])?,
                parse_rgb_component(toks[1])?,
                parse_rgb_component(toks[2])?,
            ))
        }
        "hsl" | "hsla" => {
            if toks.len() < 3 {
                return None;
            }
            let h = parse_number(toks[0])?;
            let s = parse_percent_or_unit(toks[1])?; // 0..1
            let l = parse_percent_or_unit(toks[2])?; // 0..1
            Some(hsl_to_rgb(h, s, l))
        }
        "oklch" => {
            if toks.len() < 3 {
                return None;
            }
            let l = parse_percent_or_unit(toks[0])?; // 0..1 (or %)
            let c = parse_number(toks[1])?;
            let h = parse_number(toks[2])?;
            Some(oklch_to_srgb(l, c, h))
        }
        "oklab" => {
            if toks.len() < 3 {
                return None;
            }
            let l = parse_percent_or_unit(toks[0])?;
            let a = parse_number(toks[1])?;
            let b = parse_number(toks[2])?;
            Some(oklab_to_srgb(l, a, b))
        }
        _ => None,
    }
}

/// Parse an rgb channel: `0..255` integer/float, or a percentage `0%..100%`.
pub(crate) fn parse_rgb_component(tok: &str) -> Option<u8> {
    if let Some(p) = tok.strip_suffix('%') {
        let pct = p.trim().parse::<f32>().ok()?;
        return Some((pct / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8);
    }
    let n = tok.parse::<f32>().ok()?;
    Some(n.round().clamp(0.0, 255.0) as u8)
}

/// Parse a bare number (drops a trailing `deg`/`rad`/`turn` unit on angles, treating the value
/// as already in the natural unit for the caller — degrees for hue, etc.).
pub(crate) fn parse_number(tok: &str) -> Option<f32> {
    let t = tok.trim();
    for unit in ["deg", "grad", "rad", "turn"] {
        if let Some(stripped) = t.strip_suffix(unit) {
            let v = stripped.trim().parse::<f32>().ok()?;
            return Some(match unit {
                "deg" => v,
                "grad" => v * 0.9,
                "rad" => v.to_degrees(),
                "turn" => v * 360.0,
                _ => v,
            });
        }
    }
    t.parse::<f32>().ok()
}

/// Parse a value that may be a percentage (`50%` → 0.5) or a unitless number used as-is.
/// Parse a bare `<percentage>` token (`"50%"` → `Some(50.0)`); `None` for anything else.
pub(crate) fn parse_percent(val: &str) -> Option<f32> {
    val.trim()
        .strip_suffix('%')
        .and_then(|p| p.trim().parse::<f32>().ok())
}

pub(crate) fn parse_percent_or_unit(tok: &str) -> Option<f32> {
    if let Some(p) = tok.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|v| v / 100.0);
    }
    tok.trim().parse::<f32>().ok()
}

/// HSL (h in degrees, s/l in 0..1) → sRGB 8-bit.
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        (((r1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((g1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
        (((b1 + m) * 255.0).round()).clamp(0.0, 255.0) as u8,
    )
}

/// OKLCH (L 0..1, C chroma, H degrees) → sRGB 8-bit.
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h: f32) -> (u8, u8, u8) {
    let hr = h.to_radians();
    oklab_to_srgb(l, c * hr.cos(), c * hr.sin())
}

/// OKLab (L 0..1, a, b) → sRGB 8-bit. Uses the standard OKLab→linear-sRGB matrices, then the
/// sRGB transfer function, clamped to [0, 255].
pub(crate) fn oklab_to_srgb(l: f32, a: f32, b: f32) -> (u8, u8, u8) {
    // OKLab → LMS' (cube of intermediate).
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;

    let lc = l_ * l_ * l_;
    let mc = m_ * m_ * m_;
    let sc = s_ * s_ * s_;

    // LMS → linear sRGB.
    let lr = 4.076_741_7 * lc - 3.307_711_6 * mc + 0.230_969_94 * sc;
    let lg = -1.268_438 * lc + 2.609_757_4 * mc - 0.341_319_38 * sc;
    let lb = -0.004_196_086 * lc - 0.703_418_6 * mc + 1.707_614_7 * sc;

    (srgb_encode(lr), srgb_encode(lg), srgb_encode(lb))
}

/// Linear sRGB component (0..1, may be out of range) → gamma-encoded 8-bit, clamped.
pub(crate) fn srgb_encode(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

pub(crate) fn parse_named_color(lower: &str) -> Option<(u8, u8, u8)> {
    let named = match lower {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "yellow" => (255, 255, 0),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "maroon" => (128, 0, 0),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "olive" => (128, 128, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        _ => return system_color(lower),
    };
    Some(named)
}

/// CSS system colors (a light-theme palette). Only resolved when forced colors mode is active —
/// outside it, returning `None` (unknown) keeps these keywords inert so they don't change rendering
/// for pages that aren't being run in forced colors. The exact values only need to be self-consistent
/// so a property forced to `CanvasText` renders the same as an element that names `CanvasText`.
/// Whether `lower` (already lowercased) is a CSS system color keyword — independent of forced
/// colors mode (used to detect author-specified system colors, which forced colors preserves).
pub(crate) fn is_system_color_keyword(lower: &str) -> bool {
    matches!(
        lower,
        "canvas"
            | "window"
            | "buttonface"
            | "field"
            | "infobackground"
            | "canvastext"
            | "windowtext"
            | "buttontext"
            | "fieldtext"
            | "infotext"
            | "menutext"
            | "captiontext"
            | "graytext"
            | "linktext"
            | "visitedtext"
            | "activetext"
            | "highlight"
            | "selecteditem"
            | "accentcolor"
            | "highlighttext"
            | "selecteditemtext"
            | "accentcolortext"
            | "buttonborder"
            | "threedface"
            | "buttonshadow"
            | "mark"
            | "marktext"
    )
}

pub(crate) fn system_color(lower: &str) -> Option<(u8, u8, u8)> {
    Some(match lower {
        "canvas" | "window" | "buttonface" | "field" | "infobackground" => (255, 255, 255),
        "canvastext" | "windowtext" | "buttontext" | "fieldtext" | "infotext" | "menutext"
        | "captiontext" => (0, 0, 0),
        "graytext" => (128, 128, 128),
        "linktext" => (0, 0, 238),
        "visitedtext" => (85, 26, 139),
        "activetext" => (255, 0, 0),
        "highlight" | "selecteditem" | "accentcolor" => (0, 120, 215),
        "highlighttext" | "selecteditemtext" | "accentcolortext" => (255, 255, 255),
        "buttonborder" | "threedface" | "buttonshadow" => (128, 128, 128),
        "mark" => (255, 255, 0),
        "marktext" => (0, 0, 0),
        _ => return None,
    })
}

pub(crate) fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim();
    let hx = |s: &str| u8::from_str_radix(s, 16).ok();
    match h.len() {
        // #rgb
        3 => {
            let r = hx(&h[0..1])?;
            let g = hx(&h[1..2])?;
            let b = hx(&h[2..3])?;
            Some((r * 17, g * 17, b * 17))
        }
        // #rgba — drop alpha.
        4 => {
            let r = hx(&h[0..1])?;
            let g = hx(&h[1..2])?;
            let b = hx(&h[2..3])?;
            Some((r * 17, g * 17, b * 17))
        }
        // #rrggbb
        6 => {
            let r = hx(&h[0..2])?;
            let g = hx(&h[2..4])?;
            let b = hx(&h[4..6])?;
            Some((r, g, b))
        }
        // #rrggbbaa — drop alpha.
        8 => {
            let r = hx(&h[0..2])?;
            let g = hx(&h[2..4])?;
            let b = hx(&h[4..6])?;
            Some((r, g, b))
        }
        _ => None,
    }
}
