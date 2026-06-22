use crate::*;

/// Decide whether a rule with the given raw `@media` query applies at the assumed desktop
/// viewport ([`assumed_viewport_width()`]). `None` (no media) always applies. We parse the
/// common Tailwind shapes: `screen`/`all` match, `print` does not, and single
/// `min-width`/`max-width` px thresholds are compared against the assumed width. Multiple
/// `and`-joined conditions must all pass. Unrecognized features are treated as matching
/// (best-effort, so we don't drop rules we can't fully parse).
pub(crate) fn media_applies(media: Option<&str>) -> bool {
    let query = match media {
        None => return true,
        Some(q) => q.trim(),
    };
    if query.is_empty() {
        return true;
    }
    // A comma-separated media query list matches if ANY component matches.
    query.split(',').any(media_component_matches)
}

pub(crate) fn media_component_matches(component: &str) -> bool {
    let lower = component.trim().to_ascii_lowercase();
    // Split on `and`; each part is a media type or a `(feature: value)` condition.
    for raw in lower.split(" and ") {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        // Media types.
        if part == "screen" || part == "all" {
            continue;
        }
        if part == "print" {
            return false;
        }
        // Feature conditions like `(min-width: 768px)`.
        if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            if let Some((feature, value)) = inner.split_once(':') {
                let feature = feature.trim();
                let value = value.trim();
                match feature {
                    "min-width" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_width() < px {
                                return false;
                            }
                        }
                    }
                    "max-width" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_width() > px {
                                return false;
                            }
                        }
                    }
                    "min-height" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_height() < px {
                                return false;
                            }
                        }
                    }
                    "max-height" => {
                        if let Some(px) = length_px(value) {
                            if assumed_viewport_height() > px {
                                return false;
                            }
                        }
                    }
                    // Resolution / HiDPI queries, compared against the real device pixel ratio.
                    "min-resolution"
                    | "-webkit-min-device-pixel-ratio"
                    | "min--moz-device-pixel-ratio" => {
                        if let Some(r) = resolution_dppx(value) {
                            if viewport_dpr() < r {
                                return false;
                            }
                        }
                    }
                    "max-resolution"
                    | "-webkit-max-device-pixel-ratio"
                    | "max--moz-device-pixel-ratio" => {
                        if let Some(r) = resolution_dppx(value) {
                            if viewport_dpr() > r {
                                return false;
                            }
                        }
                    }
                    "orientation" => {
                        let landscape = assumed_viewport_width() >= assumed_viewport_height();
                        if (value == "portrait" && landscape)
                            || (value == "landscape" && !landscape)
                        {
                            return false;
                        }
                    }
                    // Real OS appearance: `dark` rules apply only in Dark mode, `light` only in
                    // Light. This is what actually restyles most dark-mode-aware sites.
                    "prefers-color-scheme" => {
                        let dark = color_scheme_dark();
                        if (value == "dark" && !dark) || (value == "light" && dark) {
                            return false;
                        }
                    }
                    // Unrecognized features (other prefers-*, hover, …): treat as matching.
                    _ => {}
                }
            }
            continue;
        }
        // Bare `not`/`only` prefixes or unknown tokens: be permissive (treat as matching),
        // except an explicit leading `not` which we honor crudely.
        if part.starts_with("not ") {
            return false;
        }
    }
    true
}

/// Decide whether a rule with the given raw `@container` condition applies, evaluated against an
/// assumed container width ([`assumed_container_width()`]). Correct container sizing needs layout
/// (which runs after the cascade), so this is a pragmatic approximation that mirrors
/// [`media_applies`]: `min-width`/`max-width`/`inline-size`/`width` thresholds are compared
/// against the assumed width; multiple `and`-joined conditions must all pass. `None` (no
/// container) always applies, and unrecognized conditions are treated permissively (applied) so
/// container rules aren't dropped.
pub(crate) fn container_applies(container: Option<&str>) -> bool {
    let query = match container {
        None => return true,
        Some(q) => q.trim(),
    };
    if query.is_empty() {
        return true;
    }
    // Conditions joined by `and` must all match. We also tolerate a `(width > 400px)`-style
    // comparison form in addition to the `(min-width: 400px)` colon form.
    let lower = query.to_ascii_lowercase();
    for raw in lower.split(" and ") {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            if !container_feature_matches(inner.trim()) {
                return false;
            }
        }
        // Non-parenthesized tokens (a bare container name etc.) are ignored → permissive.
    }
    true
}

/// Evaluate a single `@container` feature condition (the text inside the parens) against
/// [`assumed_container_width()`]. Handles the colon form (`min-width: 400px`,
/// `max-inline-size: 600px`) and the range form (`width >= 400px`, `inline-size < 600px`).
/// Unrecognized features/forms → `true` (permissive).
pub(crate) fn container_feature_matches(inner: &str) -> bool {
    let w = assumed_container_width();
    // Colon form: `feature: value`.
    if let Some((feature, value)) = inner.split_once(':') {
        let feature = feature.trim();
        let value = value.trim();
        if let Some(px) = length_px(value) {
            return match feature {
                "min-width" | "min-inline-size" => w >= px,
                "max-width" | "max-inline-size" => w <= px,
                _ => true, // height/aspect/orientation/unknown → permissive
            };
        }
        return true;
    }
    // Range form: `feature OP value` where OP is one of >= <= > < =.
    for (op, less, oreq) in [
        (">=", false, true),
        ("<=", true, true),
        (">", false, false),
        ("<", true, false),
        ("=", false, false),
    ] {
        if let Some((feature, value)) = inner.split_once(op) {
            let feature = feature.trim();
            if !matches!(feature, "width" | "inline-size" | "height" | "block-size") {
                return true; // unknown feature → permissive
            }
            if matches!(feature, "height" | "block-size") {
                return true; // no assumed container height → permissive
            }
            if let Some(px) = length_px(value.trim()) {
                return match (less, oreq) {
                    (false, true) => w >= px,
                    (true, true) => w <= px,
                    (false, false) if op == "=" => (w - px).abs() < f32::EPSILON,
                    (false, false) => w > px,
                    (true, false) => w < px,
                };
            }
            return true;
        }
    }
    true // unrecognized form → permissive
}

/// Parse a media-query length to px. Supports `px`, `rem`/`em` (×16), bare numbers (px).
pub(crate) fn length_px(value: &str) -> Option<f32> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(n) = v.strip_suffix("px") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = v.strip_suffix("rem") {
        n.trim().parse::<f32>().ok().map(|x| x * 16.0)
    } else if let Some(n) = v.strip_suffix("em") {
        n.trim().parse::<f32>().ok().map(|x| x * 16.0)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Parse a resolution value into dppx (dots per `px`, i.e. the device pixel ratio): `2dppx`/`2x`
/// → 2, `192dpi` → 2 (96dpi = 1dppx), `96dpcm`→…, or a bare number (the `-webkit-*-device-pixel-ratio`
/// form) → that number.
pub(crate) fn resolution_dppx(value: &str) -> Option<f32> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(n) = v.strip_suffix("dppx").or_else(|| v.strip_suffix('x')) {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = v.strip_suffix("dpi") {
        n.trim().parse::<f32>().ok().map(|x| x / 96.0)
    } else if let Some(n) = v.strip_suffix("dpcm") {
        n.trim().parse::<f32>().ok().map(|x| x / 96.0 * 2.54)
    } else {
        v.parse::<f32>().ok()
    }
}

/// Block-level-by-default tags (mirrors the layout UA list).
pub(crate) fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "html"
            | "body"
            | "div"
            | "p"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "main"
            | "aside"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "pre"
            | "table"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "form"
            | "fieldset"
            | "figure"
            | "figcaption"
            | "address"
            | "hr"
    )
}
