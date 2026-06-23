//! Selector matching + the cascade.
//!
//! [`cascade`] walks a [`dom::Document`], matches a built-in user-agent stylesheet plus the
//! author `<style>` sheets and inline `style="…"` attributes against each element, resolves
//! the winning declarations by origin + specificity + source order, applies inheritance, and
//! returns a [`ComputedStyle`] per element [`dom::NodeId`].
//!
//! Supported selectors are *simple*: type/tag (`p`), class (`.x`), id (`#id`), the universal
//! selector (`*`), and grouped comma lists. A single compound like `p.note` (a tag plus one
//! class/id) is also handled. Descendant combinators (`div p`) are NOT supported.

mod cascade;
mod colors;
mod computed_style;
mod declaration;
mod lengths;
mod parse_props;
mod queries;
mod selector;
mod serialize;
mod values;
mod variables;

pub use cascade::*;
pub use computed_style::*;
pub use lengths::*;
pub use selector::*;
pub use serialize::*;
pub use values::*;

pub(crate) use colors::*;
pub(crate) use declaration::*;
pub(crate) use parse_props::*;
pub(crate) use queries::*;
pub(crate) use variables::*;

#[cfg(test)]
mod tests {
    use super::*;
    use dom::NodeData;
    use std::collections::HashMap;

    /// Serializes tests that read or mutate the process-global OS-appearance / root-color-scheme
    /// flags (`set_color_scheme_dark` / `root_used_scheme_dark`), which `cargo test` would otherwise
    /// run in parallel and race on. Poisoning is irrelevant (we only need exclusion), so callers
    /// ignore a poisoned guard.
    static SCHEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scheme_guard() -> std::sync::MutexGuard<'static, ()> {
        SCHEME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn elem(doc: &dom::Document, tag_and_pred: impl Fn(&dom::ElementData) -> bool) -> dom::NodeId {
        // Find first element matching predicate (depth-first).
        fn walk(
            doc: &dom::Document,
            id: dom::NodeId,
            pred: &dyn Fn(&dom::ElementData) -> bool,
        ) -> Option<dom::NodeId> {
            if let NodeData::Element(e) = &doc.get(id).data {
                if pred(e) {
                    return Some(id);
                }
            }
            for &c in &doc.get(id).children {
                if let Some(found) = walk(doc, c, pred) {
                    return Some(found);
                }
            }
            None
        }
        walk(doc, doc.root(), &tag_and_pred).expect("element not found")
    }

    // ------------------------------------------------------------------------------------------
    // Gradient / box-shadow / transform value parsing
    // ------------------------------------------------------------------------------------------

    fn grad(val: &str) -> Gradient {
        parse_gradient(val, (0, 0, 0), (0, 0, 0)).expect("expected a gradient")
    }

    #[test]
    fn linear_gradient_angle_two_stops() {
        match grad("linear-gradient(90deg, red, blue)") {
            Gradient::Linear { angle_deg, stops } => {
                assert_eq!(angle_deg, 90.0);
                assert_eq!(stops.len(), 2);
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 1.0).abs() < 1e-6);
                assert_eq!(
                    stops[0].color,
                    Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255
                    }
                );
                assert_eq!(
                    stops[1].color,
                    Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 255
                    }
                );
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn linear_gradient_to_right_with_percent_stops() {
        match grad("linear-gradient(to right, #fff 0%, #000 100%)") {
            Gradient::Linear { angle_deg, stops } => {
                assert_eq!(angle_deg, 90.0); // to right == 90deg
                assert_eq!(stops.len(), 2);
                assert_eq!(
                    stops[0].color,
                    Rgba {
                        r: 255,
                        g: 255,
                        b: 255,
                        a: 255
                    }
                );
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 1.0).abs() < 1e-6);
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn linear_gradient_distributes_three_unpositioned_stops() {
        match grad("linear-gradient(red, green, blue)") {
            Gradient::Linear { stops, .. } => {
                assert_eq!(stops.len(), 3);
                assert!((stops[0].pos - 0.0).abs() < 1e-6);
                assert!((stops[1].pos - 0.5).abs() < 1e-6);
                assert!((stops[2].pos - 1.0).abs() < 1e-6);
            }
            _ => panic!("expected linear"),
        }
    }

    #[test]
    fn radial_gradient_parses() {
        match grad("radial-gradient(red, blue)") {
            Gradient::Radial { stops } => {
                assert_eq!(stops.len(), 2);
                assert_eq!(
                    stops[0].color,
                    Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255
                    }
                );
            }
            _ => panic!("expected radial"),
        }
    }

    #[test]
    fn repeating_linear_treated_as_linear() {
        assert!(matches!(
            grad("repeating-linear-gradient(0deg, red, blue)"),
            Gradient::Linear { .. }
        ));
    }

    #[test]
    fn box_shadow_single_with_rgba() {
        let s = parse_box_shadows("2px 4px 8px rgba(0,0,0,.5)", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].dx, 2.0);
        assert_eq!(s[0].dy, 4.0);
        assert_eq!(s[0].blur, 8.0);
        assert_eq!(s[0].spread, 0.0);
        assert!(!s[0].inset);
        assert_eq!(s[0].color.a, 128);
    }

    #[test]
    fn box_shadow_two_layers() {
        let s = parse_box_shadows("2px 2px 4px black, 0 0 10px red", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].blur, 10.0);
        assert_eq!(
            s[1].color,
            Rgba {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn box_shadow_inset_with_spread() {
        let s = parse_box_shadows("inset 1px 2px 3px 4px #000", (0, 0, 0), (0, 0, 0));
        assert_eq!(s.len(), 1);
        assert!(s[0].inset);
        assert_eq!(s[0].spread, 4.0);
    }

    #[test]
    fn transform_translate_scale_composes() {
        let m = parse_transform("translate(10px, 20px) scale(2)").expect("matrix");
        // Composed: first translate then scale (translate outermost). Apply to origin (0,0):
        // result = T * S * (0,0) = (10, 20). Apply to (1,1): T*S = scale then translate.
        // x' = a*x + c*y + e = 2*1 + 0 + 10 = 12; y' = b*x + d*y + f = 0 + 2*1 + 20 = 22.
        assert_eq!(m[0], 2.0); // a (scale x)
        assert_eq!(m[3], 2.0); // d (scale y)
        assert_eq!(m[4], 10.0); // e
        assert_eq!(m[5], 20.0); // f
    }

    #[test]
    fn transform_rotate_90_matrix() {
        let m = parse_transform("rotate(90deg)").expect("matrix");
        // rotate(90deg): cos=0, sin=1 → [0, 1, -1, 0, 0, 0].
        assert!((m[0] - 0.0).abs() < 1e-5, "a={}", m[0]);
        assert!((m[1] - 1.0).abs() < 1e-5, "b={}", m[1]);
        assert!((m[2] - (-1.0)).abs() < 1e-5, "c={}", m[2]);
        assert!((m[3] - 0.0).abs() < 1e-5, "d={}", m[3]);
    }

    #[test]
    fn transform_matrix_passthrough() {
        let m = parse_transform("matrix(1,2,3,4,5,6)").expect("matrix");
        assert_eq!(m, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn transform_origin_top_left() {
        assert_eq!(parse_transform_origin("top left"), (0.0, 0.0));
        assert_eq!(parse_transform_origin("left top"), (0.0, 0.0));
        assert_eq!(parse_transform_origin("bottom right"), (1.0, 1.0));
        assert_eq!(parse_transform_origin("center"), (0.5, 0.5));
        assert_eq!(parse_transform_origin("50% 50%"), (0.5, 0.5));
    }

    #[test]
    fn gradient_applied_via_cascade_background() {
        let doc = html::parse(
            r#"<html><body><div style="background: linear-gradient(to right, red, blue)">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        assert!(map[&div].background_gradient.is_some());
        // Solid background-color must remain unset when a gradient is used.
        assert!(map[&div].background_color.is_none());
    }

    #[test]
    fn rem_resolves_against_root_font_size() {
        // html{font-size:62.5%} → root font-size = 10px, so a child's 1.6rem = 16px (not 25.6).
        let doc = html::parse(
            r#"<html style="font-size:62.5%"><body><div style="font-size:1.6rem">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        assert!(
            (map[&div].font_size - 16.0).abs() < 0.5,
            "1.6rem against a 10px root should be 16px, got {}",
            map[&div].font_size
        );
    }

    #[test]
    fn background_shorthand_with_image_url() {
        let doc = html::parse(
            r#"<html><body><div style="background: url(bg.png) no-repeat center / cover">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        let cs = &map[&div];
        assert_eq!(cs.background_image_url.as_deref(), Some("bg.png"));
        assert_eq!(cs.background_repeat, BgRepeat::NoRepeat);
        assert_eq!(cs.background_size, BgSize::Cover);
        assert_eq!(cs.background_position, (BgLen::Pct(0.5), BgLen::Pct(0.5)));
    }

    #[test]
    fn background_longhand_image_props() {
        let doc = html::parse(
            r#"<html><body><div style="background-image: url('a.svg'); background-repeat: repeat-x; background-position: right top">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let cs = &map[&elem(&doc, |e| e.tag == "div")];
        assert_eq!(cs.background_image_url.as_deref(), Some("a.svg"));
        assert_eq!(cs.background_repeat, BgRepeat::RepeatX);
        assert_eq!(cs.background_position, (BgLen::Pct(1.0), BgLen::Pct(0.0)));
    }

    #[test]
    fn box_shadow_and_transform_via_cascade() {
        let doc = html::parse(
            r#"<html><body><div style="box-shadow: 2px 4px 8px black; transform: translate(10px,20px) scale(2); transform-origin: top left">x</div></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&div].box_shadows.len(), 1);
        assert_eq!(map[&div].transform, Some([2.0, 0.0, 0.0, 2.0, 10.0, 20.0]));
        assert_eq!(map[&div].transform_origin, (0.0, 0.0));
    }

    #[test]
    fn cascade_runs_on_empty_inputs() {
        let doc = dom::Document::new();
        let map = cascade(&doc, &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn namespace_type_selector_matches_only_svg() {
        // With a default (xhtml) @namespace and a `svg` prefix, `svg|*` matches the SVG element but
        // not the HTML one; an unprefixed selector is constrained to the default (xhtml) namespace.
        let doc = html::parse("<html><body><div class=x></div><svg class=x></svg></body></html>");
        let sheet = css::parse(
            "@namespace url(http://www.w3.org/1999/xhtml); \
             @namespace svg url(http://www.w3.org/2000/svg); \
             svg|*.x { color: rgb(1, 2, 3); } \
             .x { background-color: rgb(4, 5, 6); }",
        );
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let svg = elem(&doc, |e| e.tag == "svg");
        let div = elem(&doc, |e| e.tag == "div");
        // svg|*.x matched the SVG element (color set), not the HTML div.
        assert_eq!(map[&svg].color, (1, 2, 3));
        assert_ne!(map[&div].color, (1, 2, 3));
        // The unprefixed `.x` is constrained to the default xhtml namespace -> matches the HTML div,
        // not the SVG element.
        assert_eq!(map[&div].background_color, Some((4, 5, 6)));
        assert_eq!(map[&svg].background_color, None);
    }

    #[test]
    fn registered_property_seeds_initial_value() {
        let doc = html::parse("<html><body><div id=t></div></body></html>");
        let sheet = css::parse(
            "@property --reg { syntax: \"<length>\"; inherits: false; initial-value: 7px; }",
        );
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let div = elem(&doc, |e| {
            e.attrs.get("id").map(|s| s == "t").unwrap_or(false)
        });
        // The registered property's initial value is present in the element's custom-prop env even
        // though it was never explicitly set.
        assert_eq!(
            map[&div].custom_props.get("--reg").map(String::as_str),
            Some("7px")
        );
    }

    #[test]
    fn ua_defaults_make_h1_big_and_bold() {
        let doc = html::parse("<html><body><h1>Hi</h1></body></html>");
        let map = cascade(&doc, &[]);
        let h1 = elem(&doc, |e| e.tag == "h1");
        let s = &map[&h1];
        assert_eq!(s.font_size, 32.0);
        assert!(s.bold);
        assert!(s.display_block);
    }

    #[test]
    fn ua_default_p_margin_is_one_em() {
        // The UA sheet gives <p> `margin: 1em 0`; with the default 16px font that's 16px top/bottom.
        let doc = html::parse("<html><body><p>x</p></body></html>");
        let map = cascade(&doc, &[]);
        let p = elem(&doc, |e| e.tag == "p");
        let s = &map[&p];
        assert_eq!(s.margin.top, 16.0, "p margin-top should be 1em = 16px");
        assert_eq!(s.margin.bottom, 16.0);
        assert_eq!(s.margin.left, 0.0);
        // getComputedStyle string form.
        assert_eq!(s.get_property("margin-top"), "16px");
    }

    #[test]
    fn ua_em_margin_scales_with_heading_font_size() {
        // h1 has font-size 32px and `margin: 0.67em 0` → 0.67 * 32 ≈ 21.44px (resolved against the
        // element's OWN font size, not the 16px default).
        let doc = html::parse("<html><body><h1>Hi</h1></body></html>");
        let map = cascade(&doc, &[]);
        let h1 = elem(&doc, |e| e.tag == "h1");
        let mt = map[&h1].margin.top;
        assert!(
            (mt - 0.67 * 32.0).abs() < 0.01,
            "h1 margin-top {mt} should be 0.67em of 32px"
        );
    }

    #[test]
    fn ua_ul_padding_and_list_style_and_pre_white_space() {
        let doc = html::parse(
            "<html><body><ul><li>a</li></ul><ol><li>b</li></ol><pre>code</pre></body></html>",
        );
        let map = cascade(&doc, &[]);
        let ul = elem(&doc, |e| e.tag == "ul");
        assert_eq!(map[&ul].padding.left, 40.0, "ul padding-left 40px");
        assert_eq!(map[&ul].list_style_type, ListStyleType::Disc);
        let ol = elem(&doc, |e| e.tag == "ol");
        assert_eq!(map[&ol].list_style_type, ListStyleType::Decimal);
        let pre = elem(&doc, |e| e.tag == "pre");
        assert_eq!(map[&pre].white_space, WhiteSpace::Pre);
        assert_eq!(map[&pre].get_property("white-space"), "pre");
    }

    #[test]
    fn ua_hr_has_height_and_background() {
        let doc = html::parse("<html><body><hr></body></html>");
        let map = cascade(&doc, &[]);
        let hr = elem(&doc, |e| e.tag == "hr");
        let s = &map[&hr];
        assert_eq!(
            s.height,
            Some(1.0),
            "hr should have a 1px height so it paints"
        );
        assert!(
            s.background_color.is_some(),
            "hr should have a visible background fill"
        );
    }

    #[test]
    fn lang_pseudo_class_matches_inherited_language() {
        // `:lang(zh)` and `:lang(zh-CN)` both match an element whose inherited lang is `zh-CN`;
        // `:lang(tr)` does not.
        let sheet = css::parse(
            ":lang(zh) { color: #010101 }
             :lang(zh-CN) { background-color: #020202 }
             :lang(tr) { font-size: 99px }",
        );
        let doc =
            html::parse(r#"<html><body><div lang="zh-CN"><span>x</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        // Inherited lang from the ancestor div: :lang(zh) and :lang(zh-CN) match (color/bg set),
        // :lang(tr) does not (font-size stays the inherited default, not 99px).
        assert_eq!(
            map[&span].color,
            (1, 1, 1),
            "span :lang(zh) should set color"
        );
        assert_eq!(
            map[&span].background_color,
            Some((2, 2, 2)),
            "span :lang(zh-CN) should set bg"
        );
        assert!(
            (map[&span].font_size - 99.0).abs() > 0.5,
            ":lang(tr) must not match"
        );
    }

    #[test]
    fn font_family_serializes_with_canonical_quoting() {
        // Generic families/CSS-wide keywords stay quoted; valid ident sequences unquote.
        assert_eq!(
            serialize_font_family("'Times New Roman'"),
            "Times New Roman"
        );
        assert_eq!(serialize_font_family("\"serif\""), "\"serif\"");
        assert_eq!(serialize_font_family("'34J'"), "\"34J\"");
        assert_eq!(serialize_font_family("'A  B'"), "\"A  B\"");
        assert_eq!(serialize_font_family("Veronica"), "Veronica");
        assert_eq!(
            serialize_font_family("Twisty Tie, '34J', \"serif\", Veronica, sans-serif"),
            "Twisty Tie, \"34J\", \"serif\", Veronica, sans-serif"
        );
    }

    #[test]
    fn white_space_pre_parses() {
        let doc =
            html::parse(r#"<html><body><span style="white-space: pre">x</span></body></html>"#);
        let map = cascade(&doc, &[]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].white_space, WhiteSpace::Pre);
    }

    #[test]
    fn id_beats_class_beats_type() {
        let sheet = css::parse("p { color: red } .c { color: green } #x { color: blue }");
        let doc = html::parse(r#"<html><body><p id="x" class="c">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // id selector (#x) wins → blue.
        assert_eq!(map[&p].color, (0, 0, 255));
    }

    #[test]
    fn class_beats_type() {
        let sheet = css::parse("p { color: red } .c { color: green }");
        let doc = html::parse(r#"<html><body><p class="c">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 128, 0));
    }

    #[test]
    fn inline_beats_sheet() {
        let sheet = css::parse("#x { color: blue }");
        let doc = html::parse(r#"<html><body><p id="x" style="color: red">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn color_and_font_size_inherit_to_children() {
        let sheet = css::parse("#wrap { color: #ff0000; font-size: 24px }");
        let doc =
            html::parse(r#"<html><body><div id="wrap"><span>child</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (255, 0, 0));
        assert_eq!(map[&span].font_size, 24.0);
    }

    #[test]
    fn display_none_propagates_to_subtree() {
        let sheet = css::parse("#h { display: none }");
        let doc =
            html::parse(r#"<html><body><div id="h"><p>hidden</p></div><p>shown</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let hidden_div = elem(&doc, |e| e.id() == Some("h"));
        assert!(map[&hidden_div].display_none);
        // The nested <p> inherits hidden-ness.
        let inner = elem(&doc, |e| {
            e.tag == "p"
            // the hidden one is the first <p>
        });
        // First matching p in doc order is the hidden one.
        assert!(map[&inner].display_none);
    }

    #[test]
    fn compound_selector_matches() {
        let sheet = css::parse("p.note { color: orange }");
        let doc = html::parse(r#"<html><body><p class="note">a</p><p>b</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let note = elem(&doc, |e| e.tag == "p" && e.classes().any(|c| c == "note"));
        assert_eq!(map[&note].color, (255, 165, 0));
    }

    #[test]
    fn named_and_hex_colors_parse() {
        assert_eq!(parse_color("#f00"), Some((255, 0, 0)));
        assert_eq!(parse_color("#00ff00"), Some((0, 255, 0)));
        assert_eq!(parse_color("blue"), Some((0, 0, 255)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn font_sizes_parse() {
        assert_eq!(parse_font_size("20px", 16.0), Some(20.0));
        assert_eq!(parse_font_size("12pt", 16.0), Some(16.0));
        assert_eq!(parse_font_size("2em", 16.0), Some(32.0));
    }

    #[test]
    fn margin_shorthand_one_value() {
        assert_eq!(parse_edges_shorthand("10px", 16.0), Some(Edges::all(10.0)));
    }

    #[test]
    fn margin_shorthand_two_values() {
        // vertical horizontal
        assert_eq!(
            parse_edges_shorthand("10px 20px", 16.0),
            Some(Edges {
                top: 10.0,
                bottom: 10.0,
                right: 20.0,
                left: 20.0
            })
        );
    }

    #[test]
    fn margin_shorthand_four_values() {
        // top right bottom left
        assert_eq!(
            parse_edges_shorthand("1px 2px 3px 4px", 16.0),
            Some(Edges {
                top: 1.0,
                right: 2.0,
                bottom: 3.0,
                left: 4.0
            })
        );
    }

    #[test]
    fn margin_applied_via_cascade() {
        let sheet = css::parse("p { margin: 5px 10px }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(
            map[&p].margin,
            Edges {
                top: 5.0,
                bottom: 5.0,
                right: 10.0,
                left: 10.0
            }
        );
    }

    #[test]
    fn per_side_override_and_specificity() {
        // The longhand override and a higher-specificity rule both apply on top of shorthand.
        let sheet = css::parse("p { margin: 4px; margin-left: 12px } .x { margin-top: 20px }");
        let doc = html::parse(r#"<html><body><p class="x">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        let m = map[&p].margin;
        assert_eq!(m.left, 12.0); // longhand overrode shorthand
        assert_eq!(m.top, 20.0); // higher specificity .x rule wins
        assert_eq!(m.right, 4.0); // untouched shorthand value
        assert_eq!(m.bottom, 4.0);
    }

    #[test]
    fn padding_shorthand_three_values() {
        let sheet = css::parse("div { padding: 1px 2px 3px }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(
            map[&d].padding,
            Edges {
                top: 1.0,
                right: 2.0,
                left: 2.0,
                bottom: 3.0
            }
        );
    }

    #[test]
    fn border_shorthand_width_and_color() {
        let sheet = css::parse("div { border: 2px solid #ff0000 }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].border, Edges::all(2.0));
        assert_eq!(map[&d].border_color, (255, 0, 0));
    }

    #[test]
    fn border_none_is_zero() {
        let sheet = css::parse("div { border: none }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].border, Edges::all(0.0));
    }

    #[test]
    fn ua_table_display_values() {
        // The UA stylesheet maps table tags to their table-* display values, and styles <th>.
        let doc = html::parse(
            r#"<html><body><table>
                <caption>Cap</caption>
                <thead><tr><th>H</th></tr></thead>
                <tbody><tr><td>D</td></tr></tbody>
                <tfoot><tr><td>F</td></tr></tfoot>
                <colgroup><col></colgroup>
            </table></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let d = |tag: &str| map[&elem(&doc, |e| e.tag == tag)].display;
        assert_eq!(d("table"), Display::Table);
        assert_eq!(d("tr"), Display::TableRow);
        assert_eq!(d("td"), Display::TableCell);
        assert_eq!(d("th"), Display::TableCell);
        assert_eq!(d("thead"), Display::TableHeaderGroup);
        assert_eq!(d("tbody"), Display::TableRowGroup);
        assert_eq!(d("tfoot"), Display::TableFooterGroup);
        assert_eq!(d("caption"), Display::TableCaption);
        assert_eq!(d("colgroup"), Display::TableColumnGroup);
        assert_eq!(d("col"), Display::TableColumn);
        // <th> defaults: bold + centered + 1px padding (the cells get a little padding).
        let th = map[&elem(&doc, |e| e.tag == "th")].clone();
        assert!(th.bold, "th should be bold");
        assert_eq!(th.text_align, TextAlign::Center);
        assert_eq!(th.padding, Edges::all(1.0));
        // getComputedStyle reports the table display string.
        assert_eq!(
            map[&elem(&doc, |e| e.tag == "table")].get_property("display"),
            "table"
        );
    }

    #[test]
    fn width_parses_to_some() {
        let sheet = css::parse("div { width: 200px; height: auto }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].width, Some(200.0));
        assert_eq!(map[&d].height, None);
    }

    #[test]
    fn percentage_and_garbage_ignored() {
        assert_eq!(parse_length("50%"), None);
        assert_eq!(parse_length("auto"), None);
        assert_eq!(parse_length("garbage"), None);
        assert_eq!(parse_length("12px"), Some(12.0));
        assert_eq!(parse_length("0"), Some(0.0));
    }

    #[test]
    fn display_and_position_parse() {
        let sheet = css::parse(
            "#a { display: flex; position: relative } \
             #b { display: grid } \
             #c { display: inline-block; position: absolute }",
        );
        let doc = html::parse(
            r#"<html><body><div id="a"></div><div id="b"></div><span id="c"></span></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert_eq!(map[&a].display, Display::Flex);
        assert_eq!(map[&a].position, Position::Relative);
        assert_eq!(map[&b].display, Display::Grid);
        assert_eq!(map[&c].display, Display::InlineBlock);
        assert_eq!(map[&c].position, Position::Absolute);
    }

    #[test]
    fn display_default_per_tag() {
        let doc = html::parse(r#"<html><body><div></div><span></span></body></html>"#);
        let map = cascade(&doc, &[]);
        let div = elem(&doc, |e| e.tag == "div");
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&div].display, Display::Block);
        assert!(map[&div].display_block);
        assert_eq!(map[&span].display, Display::Inline);
        assert!(!map[&span].display_block);
    }

    #[test]
    fn flex_shorthand_expands() {
        assert_eq!(parse_flex_test("1"), (1.0, 1.0, Some(0.0)));
        assert_eq!(parse_flex_test("2 3 40px"), (2.0, 3.0, Some(40.0)));
        assert_eq!(parse_flex_test("none"), (0.0, 0.0, None));
        assert_eq!(parse_flex_test("auto"), (1.0, 1.0, None));
        assert_eq!(parse_flex_test("0 0 100px"), (0.0, 0.0, Some(100.0)));
    }

    fn parse_flex_test(v: &str) -> (f32, f32, Option<f32>) {
        let mut s = ComputedStyle::default();
        apply_flex_shorthand(&mut s, v);
        (s.flex_grow, s.flex_shrink, s.flex_basis)
    }

    #[test]
    fn flex_grow_and_basis_longhand() {
        let sheet = css::parse("#a { flex-grow: 2; flex-basis: 50px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].flex_grow, 2.0);
        assert_eq!(map[&a].flex_basis, Some(50.0));
        assert_eq!(map[&a].flex_shrink, 1.0); // default
    }

    #[test]
    fn gap_one_and_two_values() {
        assert_eq!(parse_gap("10px"), Some((10.0, 10.0)));
        assert_eq!(parse_gap("10px 20px"), Some((10.0, 20.0)));
    }

    #[test]
    fn grid_template_columns_track_parsing() {
        assert_eq!(
            parse_track_list("100px 1fr 50% auto"),
            vec![
                TrackSize::Px(100.0),
                TrackSize::Fr(1.0),
                TrackSize::Pct(50.0),
                TrackSize::Auto
            ]
        );
        // repeat() expansion.
        assert_eq!(
            parse_track_list("repeat(3, 1fr)"),
            vec![TrackSize::Fr(1.0), TrackSize::Fr(1.0), TrackSize::Fr(1.0)]
        );
    }

    #[test]
    fn insets_and_z_index_parse() {
        let sheet =
            css::parse("#a { top: 10px; left: 20px; right: auto; bottom: 5px; z-index: 7 }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(10.0));
        assert_eq!(map[&a].left, Some(20.0));
        assert_eq!(map[&a].right, None); // auto
        assert_eq!(map[&a].bottom, Some(5.0));
        assert_eq!(map[&a].z_index, Some(7));
    }

    #[test]
    fn grid_placement_parses() {
        assert_eq!(
            parse_grid_placement("1 / 3"),
            Some(GridPlacement {
                start: Some(1),
                end: GridEnd::Line(3)
            })
        );
        assert_eq!(
            parse_grid_placement("2 / span 2"),
            Some(GridPlacement {
                start: Some(2),
                end: GridEnd::Span(2)
            })
        );
        assert_eq!(
            parse_grid_placement("span 3"),
            Some(GridPlacement {
                start: None,
                end: GridEnd::Span(3)
            })
        );
    }

    #[test]
    fn rgb_function_parses() {
        assert_eq!(parse_color("rgb(255 0 0)"), Some((255, 0, 0)));
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some((255, 0, 0)));
        assert_eq!(parse_color("rgba(0, 0, 255, 0.5)"), Some((0, 0, 255)));
        assert_eq!(parse_color("rgb(100% 0% 0%)"), Some((255, 0, 0)));
    }

    #[test]
    fn hsl_function_parses_to_red() {
        let (r, g, b) = parse_color("hsl(0 100% 50%)").unwrap();
        assert!(r > 250, "r={r}");
        assert!(g < 5 && b < 5, "g={g} b={b}");
    }

    #[test]
    fn oklch_red_is_roughly_red() {
        // Tailwind-ish red: high lightness/chroma at ~29deg hue.
        let (r, g, b) = parse_color("oklch(0.628 0.2577 29.23)").unwrap();
        assert!(r > 200, "expected high R, got {r}");
        assert!(g < 120 && b < 120, "expected low-ish G/B, got g={g} b={b}");
        assert!(r > g && r > b, "red should dominate: {r},{g},{b}");
    }

    #[test]
    fn oklab_parses() {
        // Should not panic and stay in range.
        let c = parse_color("oklab(0.5 0.1 0.1)");
        assert!(c.is_some());
    }

    #[test]
    fn hex_alpha_drops_alpha() {
        assert_eq!(parse_color("#ff000080"), Some((255, 0, 0)));
        assert_eq!(parse_color("#f008"), Some((255, 0, 0)));
    }

    #[test]
    fn transparent_yields_no_color() {
        assert_eq!(parse_color("transparent"), None);
    }

    #[test]
    fn var_resolves_from_root_to_descendant() {
        // :root sets --x; a descendant uses color: var(--x).
        let sheet = css::parse(":root { --x: #0000ff } span { color: var(--x) }");
        let doc = html::parse(r#"<html><body><div><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (0, 0, 255));
    }

    #[test]
    fn var_fallback_used_when_undefined() {
        let sheet = css::parse("p { color: var(--missing, #00ff00) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn var_referencing_var_resolves() {
        let sheet = css::parse(":root { --a: #ff0000; --b: var(--a) } p { color: var(--b) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn cyclic_var_does_not_hang() {
        let sheet = css::parse(":root { --a: var(--b); --b: var(--a) } p { color: var(--a) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        // Should terminate (depth cap) and simply not set a color.
        let _ = cascade(&doc, &[sheet]);
    }

    #[test]
    fn current_color_uses_element_color() {
        let sheet = css::parse("p { color: #ff0000; border: 1px solid currentColor }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].border_color, (255, 0, 0));
    }

    #[test]
    fn inherit_keyword_takes_parent_color() {
        let sheet = css::parse("#wrap { color: #ff0000 } span { color: inherit }");
        let doc = html::parse(r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (255, 0, 0));
    }

    #[test]
    fn media_min_width_rule_applies_at_desktop() {
        // min-width:768px applies at the assumed 1280px viewport.
        let sheet = css::parse("@media (min-width: 768px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn media_min_width_above_viewport_does_not_apply() {
        let sheet =
            css::parse("p { color: #ff0000 } @media (min-width: 2000px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 2000px > 1280px assumed width, so the media rule does not apply: stays red.
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn media_prefers_color_scheme_tracks_os_appearance() {
        let _g = scheme_guard();
        let sheet = css::parse(
            "p { color: rgb(10,20,30) } \
             @media (prefers-color-scheme: dark) { p { color: rgb(1,2,3) } } \
             @media (prefers-color-scheme: light) { p { color: rgb(4,5,6) } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);

        // Dark: the `dark` rule applies, the `light` rule is dropped.
        set_color_scheme_dark(true);
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(
            map[&p].color,
            (1, 2, 3),
            "dark rule should win in Dark mode"
        );

        // Light: the `light` rule applies, the `dark` rule is dropped.
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[sheet]);
        assert_eq!(
            map[&p].color,
            (4, 5, 6),
            "light rule should win in Light mode"
        );
    }

    #[test]
    fn color_scheme_parses() {
        assert_eq!(parse_color_scheme("normal"), Some(ColorScheme::Normal));
        assert_eq!(parse_color_scheme("light"), Some(ColorScheme::Light));
        assert_eq!(parse_color_scheme("dark"), Some(ColorScheme::Dark));
        assert_eq!(
            parse_color_scheme("light dark"),
            Some(ColorScheme::LightDark)
        );
        assert_eq!(
            parse_color_scheme("dark light"),
            Some(ColorScheme::LightDark)
        );
        // `only` and unknown idents are ignored.
        assert_eq!(parse_color_scheme("only dark"), Some(ColorScheme::Dark));
        assert_eq!(parse_color_scheme("dark only"), Some(ColorScheme::Dark));
        assert_eq!(parse_color_scheme("foo bar"), Some(ColorScheme::Normal));
        assert_eq!(parse_color_scheme(""), None);
    }

    #[test]
    fn color_scheme_resolves_dark() {
        assert!(ColorScheme::Dark.resolves_dark(false));
        assert!(ColorScheme::Dark.resolves_dark(true));
        assert!(!ColorScheme::Light.resolves_dark(true));
        assert!(!ColorScheme::Normal.resolves_dark(true));
        assert!(ColorScheme::LightDark.resolves_dark(true));
        assert!(!ColorScheme::LightDark.resolves_dark(false));
    }

    #[test]
    fn root_dark_scheme_themes_default_text() {
        let _g = scheme_guard();
        // :root { color-scheme: dark } → root used scheme dark → default UA text light. The map's
        // colors are captured during the cascade (which holds CASCADE_LOCK, so the root-scheme
        // global it writes is the one it reads back), so they're race-free to assert on.
        let sheet = css::parse(":root { color-scheme: dark }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // Default (UA) text color is now light, not black.
        assert_eq!(map[&p].color, (0xe8, 0xe8, 0xe8));
    }

    #[test]
    fn root_light_scheme_keeps_black_text() {
        let _g = scheme_guard();
        let sheet = css::parse(":root { color-scheme: light }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        set_color_scheme_dark(true); // OS dark, but page opts only into light
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 0, 0));
    }

    #[test]
    fn meta_color_scheme_dark_opts_in() {
        let _g = scheme_guard();
        // <meta name="color-scheme" content="dark"> with no CSS property.
        let doc = html::parse(
            r#"<html><head><meta name="color-scheme" content="dark"></head><body><p>t</p></body></html>"#,
        );
        set_color_scheme_dark(false);
        let map = cascade(&doc, &[]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0xe8, 0xe8, 0xe8));
    }

    #[test]
    fn color_scheme_get_property_serializes() {
        let mut s = ComputedStyle {
            color_scheme: ColorScheme::LightDark,
            ..Default::default()
        };
        assert_eq!(s.get_property("color-scheme"), "light dark");
        s.color_scheme = ColorScheme::Dark;
        assert_eq!(s.get_property("color-scheme"), "dark");
    }

    #[test]
    fn media_max_width_below_viewport_does_not_apply() {
        let sheet =
            css::parse("p { color: #ff0000 } @media (max-width: 600px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn min_max_width_height_parse_px_and_pct() {
        let sheet = css::parse(
            "#a { max-width: 200px; min-width: 50%; max-height: none; min-height: 30px }",
        );
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].max_width, Some(SizeConstraint::Px(200.0)));
        assert_eq!(map[&a].min_width, Some(SizeConstraint::Pct(0.5)));
        assert_eq!(map[&a].max_height, None); // none → unset
        assert_eq!(map[&a].min_height, Some(SizeConstraint::Px(30.0)));
    }

    #[test]
    fn inset_shorthand_sets_four_sides() {
        let sheet = css::parse("#a { inset: 1px 2px 3px 4px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(1.0));
        assert_eq!(map[&a].right, Some(2.0));
        assert_eq!(map[&a].bottom, Some(3.0));
        assert_eq!(map[&a].left, Some(4.0));
    }

    #[test]
    fn inset_block_and_inline_map_to_physical() {
        let sheet = css::parse("#a { inset-block: 5px 6px; inset-inline: 7px 8px }");
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].top, Some(5.0));
        assert_eq!(map[&a].bottom, Some(6.0));
        assert_eq!(map[&a].left, Some(7.0));
        assert_eq!(map[&a].right, Some(8.0));
    }

    #[test]
    fn padding_and_margin_block_inline() {
        let sheet = css::parse(
            "#a { padding-block: 4px; padding-inline: 8px 12px; margin-block: 2px 3px }",
        );
        let doc = html::parse(r#"<html><body><div id="a"></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        assert_eq!(map[&a].padding.top, 4.0);
        assert_eq!(map[&a].padding.bottom, 4.0);
        assert_eq!(map[&a].padding.left, 8.0);
        assert_eq!(map[&a].padding.right, 12.0);
        assert_eq!(map[&a].margin.top, 2.0);
        assert_eq!(map[&a].margin.bottom, 3.0);
    }

    #[test]
    fn line_height_unitless_px_percent() {
        // unitless 1.5 × 16 = 24
        assert_eq!(parse_line_height("1.5", 16.0), Some(24.0));
        // px direct
        assert_eq!(parse_line_height("20px", 16.0), Some(20.0));
        // percent of font-size: 150% × 20 = 30
        assert_eq!(parse_line_height("150%", 20.0), Some(30.0));
        // em × font-size
        assert_eq!(parse_line_height("2em", 10.0), Some(20.0));
        assert_eq!(parse_line_height("normal", 16.0), None);
    }

    #[test]
    fn line_height_inherits_resolved_px() {
        let sheet = css::parse("#wrap { font-size: 20px; line-height: 1.5 }");
        let doc = html::parse(r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let wrap = elem(&doc, |e| e.id() == Some("wrap"));
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&wrap].line_height, Some(30.0)); // 1.5 × 20
        assert_eq!(map[&span].line_height, Some(30.0)); // inherited resolved px
    }

    #[test]
    fn text_transform_parses_and_inherits() {
        let sheet = css::parse("#wrap { text-transform: uppercase }");
        let doc = html::parse(r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let wrap = elem(&doc, |e| e.id() == Some("wrap"));
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&wrap].text_transform, TextTransform::Uppercase);
        assert_eq!(map[&span].text_transform, TextTransform::Uppercase);
    }

    #[test]
    fn text_decoration_underline_flag() {
        let sheet = css::parse("#a { text-decoration: underline } #b { text-decoration: line-through } #c { text-decoration: none }");
        let doc = html::parse(
            r#"<html><body><a id="a">x</a><a id="b">y</a><a id="c">z</a></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert!(map[&a].underline);
        assert!(!map[&a].line_through);
        assert!(map[&b].line_through);
        assert!(!map[&c].underline && !map[&c].line_through);
    }

    #[test]
    fn ua_inline_text_defaults_cascade() {
        // The UA stylesheet styles inline text elements; verify a representative set reaches
        // the computed style (and is reported by getComputedStyle).
        let doc = html::parse(
            r##"<html><body>
                 <a href="#">link</a>
                 <s>strike</s>
                 <del>del</del>
                 <ins>ins</ins>
                 <mark>mark</mark>
                 <cite>cite</cite>
                 <abbr title="t">abbr</abbr>
                 <sup>2</sup>
                 <sub>2</sub>
                 <small>small</small>
               </body></html>"##,
        );
        let map = cascade(&doc, &[]);
        let g = |tag: &str| {
            let id = elem(&doc, |e| e.tag == tag);
            &map[&id]
        };
        // <a>: blue + underline.
        let a = g("a");
        assert!(a.underline, "a should be underlined");
        assert_eq!(a.color, (0x00, 0x00, 0xee), "a should be link blue");
        assert_eq!(a.get_property("text-decoration"), "underline");
        // <s>/<del>: line-through.
        assert!(g("s").line_through);
        assert!(g("del").line_through);
        assert_eq!(g("s").get_property("text-decoration"), "line-through");
        // <ins>: underline.
        assert!(g("ins").underline);
        // <mark>: yellow bg, black text.
        assert_eq!(g("mark").background_color, Some((0xff, 0xff, 0x00)));
        assert_eq!(g("mark").color, (0, 0, 0));
        assert_eq!(
            g("mark").get_property("background-color"),
            "rgb(255, 255, 0)"
        );
        // <cite>: italic.
        assert!(g("cite").italic);
        // <abbr title>: underline.
        assert!(g("abbr").underline);
        // <sup>/<sub>: smaller font + vertical-align.
        assert!(
            g("sup").font_size < 16.0,
            "sup should be smaller, got {}",
            g("sup").font_size
        );
        assert_eq!(g("sup").vertical_align, VerticalAlign::Super);
        assert_eq!(g("sub").vertical_align, VerticalAlign::Sub);
        assert_eq!(g("sup").get_property("vertical-align"), "super");
        // <small>: smaller font.
        assert!(
            g("small").font_size < 16.0,
            "small should be smaller, got {}",
            g("small").font_size
        );
    }

    #[test]
    fn font_size_relative_keywords() {
        assert_eq!(parse_font_size("smaller", 16.0), Some(16.0 / 1.2));
        assert_eq!(parse_font_size("larger", 10.0), Some(12.0));
    }

    #[test]
    fn q_quote_marks_via_pseudo_content() {
        let doc = html::parse(r#"<html><body><q>quote</q></body></html>"#);
        let map = cascade(&doc, &[]);
        let q = elem(&doc, |e| e.tag == "q");
        let before = map[&q].before.as_ref().expect("q::before should exist");
        let after = map[&q].after.as_ref().expect("q::after should exist");
        assert_eq!(before.content.as_deref(), Some("\u{201C}"));
        assert_eq!(after.content.as_deref(), Some("\u{201D}"));
    }

    #[test]
    fn opacity_clamps_to_unit_range() {
        let sheet = css::parse("#a { opacity: 0.5 } #b { opacity: 2 } #c { opacity: -1 }");
        let doc = html::parse(
            r#"<html><body><div id="a"></div><div id="b"></div><div id="c"></div></body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let a = elem(&doc, |e| e.id() == Some("a"));
        let b = elem(&doc, |e| e.id() == Some("b"));
        let c = elem(&doc, |e| e.id() == Some("c"));
        assert_eq!(map[&a].opacity, 0.5);
        assert_eq!(map[&b].opacity, 1.0);
        assert_eq!(map[&c].opacity, 0.0);
    }

    #[test]
    fn border_radius_one_and_four_values() {
        assert_eq!(parse_border_radius("8px"), Some(8.0));
        // four values → first is used uniformly
        assert_eq!(parse_border_radius("4px 8px 12px 16px"), Some(4.0));
        // elliptical syntax: use horizontal radii before `/`
        assert_eq!(parse_border_radius("10px / 20px"), Some(10.0));
    }

    #[test]
    fn opacity_does_not_inherit() {
        let sheet = css::parse("#wrap { opacity: 0.5 }");
        let doc = html::parse(r#"<html><body><div id="wrap"><span>t</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].opacity, 1.0);
    }

    // --- Math functions: min/max/clamp/calc -------------------------------------------------

    #[test]
    fn eval_min_max_clamp() {
        assert_eq!(eval_length("min(10px, 20px)", 16.0), Some(10.0));
        assert_eq!(eval_length("max(10px, 20px, 5px)", 16.0), Some(20.0));
        // clamped up to lo
        assert_eq!(eval_length("clamp(5px, 2px, 10px)", 16.0), Some(5.0));
        // value within range
        assert_eq!(eval_length("clamp(5px, 8px, 10px)", 16.0), Some(8.0));
        // clamped down to hi
        assert_eq!(eval_length("clamp(5px, 80px, 10px)", 16.0), Some(10.0));
    }

    #[test]
    fn eval_calc_precedence_and_units() {
        // 2rem(32) + 10px = 42
        assert_eq!(eval_length("calc(2rem + 10px)", 16.0), Some(42.0));
        // precedence: 2 + 3*4px = 14
        assert_eq!(eval_length("calc(2px + 3 * 4px)", 16.0), Some(14.0));
        // parens override precedence: (2 + 3) * 4 = 20
        assert_eq!(eval_length("calc((2px + 3px) * 4)", 16.0), Some(20.0));
        // em resolves against the passed font size
        assert_eq!(eval_length("calc(2em)", 10.0), Some(20.0));
        // vw = 1280/100 * 10 = 128
        assert_eq!(eval_length("calc(10vw)", 16.0), Some(128.0));
    }

    #[test]
    fn eval_nested_functions() {
        // calc(1px*100) = 100, clamped to [1rem=16, 50] → 50
        assert_eq!(
            eval_length("clamp(1rem, calc(1px * 100), 50px)", 16.0),
            Some(50.0)
        );
        // nested min inside max
        assert_eq!(eval_length("max(min(30px, 10px), 5px)", 16.0), Some(10.0));
    }

    #[test]
    fn eval_unknown_falls_back_to_none() {
        assert_eq!(eval_length("calc(2px + 3foo)", 16.0), None); // unknown unit
        assert_eq!(eval_length("min()", 16.0), None);
        assert_eq!(eval_length("calc(1px /)", 16.0), None); // malformed
        assert_eq!(eval_length("clamp(1px, 2px)", 16.0), None); // wrong arity
    }

    #[test]
    fn plain_lengths_still_parse_identically() {
        assert_eq!(parse_length("12px"), Some(12.0));
        assert_eq!(parse_length("0"), Some(0.0));
        assert_eq!(parse_length("50%"), None);
        // math wired into parse_length
        assert_eq!(parse_length("min(10px, 20px)"), Some(10.0));
        assert_eq!(parse_length("calc(2rem + 10px)"), Some(42.0));
    }

    #[test]
    fn font_size_clamp_resolves_on_node() {
        let sheet = css::parse("p { font-size: clamp(10px, 2vw, 30px) }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 2vw = 25.6, within [10,30] → 25.6
        assert!(
            (map[&p].font_size - 25.6).abs() < 0.01,
            "got {}",
            map[&p].font_size
        );
    }

    #[test]
    fn width_calc_resolves_on_node() {
        let sheet = css::parse("div { width: calc(100px + 1rem) }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].width, Some(116.0));
    }

    #[test]
    fn max_width_max_function_resolves() {
        let sheet = css::parse("div { max-width: max(200px, 50px) }");
        let doc = html::parse(r#"<html><body><div>t</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].max_width, Some(SizeConstraint::Px(200.0)));
    }

    // --- Container queries -------------------------------------------------------------------

    #[test]
    fn container_min_width_rule_applies_at_assumed_width() {
        // 400px <= assumed container width (1000px) → applies.
        let sheet = css::parse("@container (min-width: 400px) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn container_min_width_above_assumed_does_not_apply() {
        let sheet = css::parse(
            "p { color: #ff0000 } @container (min-width: 5000px) { p { color: #00ff00 } }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // 5000px > 1000px assumed container width → rule does not apply: stays red.
        assert_eq!(map[&p].color, (255, 0, 0));
    }

    #[test]
    fn container_unrecognized_condition_is_permissive() {
        // An aspect/orientation-style condition we don't model → rule still applies.
        let sheet = css::parse("@container (orientation: landscape) { p { color: #00ff00 } }");
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 255, 0));
    }

    #[test]
    fn box_props_do_not_inherit() {
        let sheet = css::parse("#wrap { margin: 30px; padding: 10px; width: 300px }");
        let doc =
            html::parse(r#"<html><body><div id="wrap"><span>child</span></div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].margin, Edges::default());
        assert_eq!(map[&span].padding, Edges::default());
        assert_eq!(map[&span].width, None);
    }

    /// Brute-force reference: for one element, the set of `(origin, order, max_specificity)`
    /// the *original* O(all-rules) scan would have produced — one entry per rule, max
    /// specificity over its comma selectors, media/container gated, exactly as the pre-index
    /// code did. Used to cross-check the index produces the identical match set.
    fn naive_matches(
        doc: &dom::Document,
        nid: dom::NodeId,
        ua: &css::Stylesheet,
        author: &[css::Stylesheet],
    ) -> Vec<(u8, usize, u32)> {
        let mut out = Vec::new();
        let mut order = 0usize;
        for rule in &ua.rules {
            if media_applies(rule.media.as_deref()) && container_applies(rule.container.as_deref())
            {
                if let Some(spec) = rule_specificity(&rule.selectors, doc, nid) {
                    out.push((0u8, order, spec));
                }
            }
            order += 1;
        }
        for sheet in author {
            for rule in &sheet.rules {
                if media_applies(rule.media.as_deref())
                    && container_applies(rule.container.as_deref())
                {
                    if let Some(spec) = rule_specificity(&rule.selectors, doc, nid) {
                        out.push((1u8, order, spec));
                    }
                }
                order += 1;
            }
        }
        out.sort();
        out
    }

    /// The same query the indexed cascade runs, surfaced as `(origin, order, max_spec)` so it
    /// can be compared against `naive_matches`.
    fn indexed_matches(
        doc: &dom::Document,
        nid: dom::NodeId,
        el: &dom::ElementData,
        index: &SelectorIndex,
    ) -> Vec<(u8, usize, u32)> {
        let mut best: HashMap<usize, (u8, u32)> = HashMap::new();
        let mut consider = |e: &Entry| {
            if complex_matches(doc, nid, &e.compiled.selector) {
                best.entry(e.order)
                    .and_modify(|(_, s)| *s = (*s).max(e.compiled.specificity))
                    .or_insert((e.origin, e.compiled.specificity));
            }
        };
        if let Some(id) = el.id() {
            if let Some(b) = index.by_id.get(id) {
                for e in b {
                    consider(e);
                }
            }
        }
        for class in el.classes() {
            if let Some(b) = index.by_class.get(class) {
                for e in b {
                    consider(e);
                }
            }
        }
        if let Some(b) = index.by_type.get(&el.tag.to_lowercase()) {
            for e in b {
                consider(e);
            }
        }
        for e in &index.universal {
            consider(e);
        }
        let mut out: Vec<_> = best
            .into_iter()
            .map(|(order, (origin, spec))| (origin, order, spec))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn indexed_match_set_equals_naive_for_varied_selectors() {
        // Exercise id / class / type / universal / :root / multi-class / comma / div.foo.
        let sheet = css::parse(
            "* { color: #111111 }
             :root { color: #222222 }
             div { color: #333333 }
             .foo { color: #444444 }
             div.foo { color: #555555 }
             .foo.bar { color: #666666 }
             #hero, .promo { color: #777777 }
             #hero { font-size: 20px }
             p, .foo, #hero { letter-spacing: 1px }
             a > b { color: #888888 }
             [data-x] { color: #999999 }",
        );
        let ua = user_agent_stylesheet();
        let author = [sheet];
        let index = SelectorIndex::build(&ua, &author);

        let doc = html::parse(
            r#"<html><body>
                 <div id="hero" class="foo bar promo">A</div>
                 <div class="foo">B</div>
                 <p class="promo">C</p>
                 <span>D</span>
                 <a><b>E</b></a>
               </body></html>"#,
        );
        // Check every element in the tree, not just a handful.
        fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut Vec<dom::NodeId>) {
            if let NodeData::Element(_) = &doc.get(id).data {
                out.push(id);
            }
            for &c in &doc.get(id).children {
                walk(doc, c, out);
            }
        }
        let mut ids = Vec::new();
        walk(&doc, doc.root(), &mut ids);
        assert!(ids.len() >= 7);
        for id in ids {
            if let NodeData::Element(el) = &doc.get(id).data {
                assert_eq!(
                    indexed_matches(&doc, id, el, &index),
                    naive_matches(&doc, id, &ua, &author),
                    "match set diverged for <{}>",
                    el.tag
                );
            }
        }
    }

    #[test]
    fn varied_selector_cascade_values() {
        let sheet = css::parse(
            ":root { color: #010101 }
             * { letter-spacing: 0 }
             div { color: #020202 }
             .foo { color: #030303 }
             div.foo { color: #0a0b0c }
             .foo.bar { font-size: 21px }
             #hero, .promo { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div id="hero" class="foo bar promo">A</div>
                 <div class="foo">B</div>
                 <span class="bar">C</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        // <div id=hero class="foo bar promo">: div.foo (spec 11) beats .foo (10) and div (1)
        // → color #0a0b0c; .foo.bar sets font-size 21; #hero/.promo → bold.
        let hero = elem(&doc, |e| e.id() == Some("hero"));
        assert_eq!(map[&hero].color, (10, 11, 12));
        assert_eq!(map[&hero].font_size, 21.0);
        assert!(map[&hero].bold);
        // <div class="foo">: div.foo doesn't match (needs tag div — it does), wait it's a div
        // so div.foo matches → #0a0b0c too.
        // <span class="bar">: only `*` and `.foo.bar` (no, needs foo) — none color it, so it
        // inherits the html/body UA color.
        let span = elem(&doc, |e| e.tag == "span");
        assert_eq!(map[&span].color, (0, 0, 0));
        assert!(!span_is_bold(&map, &doc));
    }

    fn span_is_bold(map: &HashMap<dom::NodeId, ComputedStyle>, doc: &dom::Document) -> bool {
        let span = elem(doc, |e| e.tag == "span");
        map[&span].bold
    }

    // ====================================================================================
    // Complex selector engine tests
    // ====================================================================================

    /// Find the nth (0-based) element matching a predicate, depth-first.
    fn elem_nth(
        doc: &dom::Document,
        n: usize,
        pred: impl Fn(&dom::ElementData) -> bool,
    ) -> dom::NodeId {
        fn walk(
            doc: &dom::Document,
            id: dom::NodeId,
            pred: &dyn Fn(&dom::ElementData) -> bool,
            out: &mut Vec<dom::NodeId>,
        ) {
            if let NodeData::Element(e) = &doc.get(id).data {
                if pred(e) {
                    out.push(id);
                }
            }
            for &c in &doc.get(id).children {
                walk(doc, c, pred, out);
            }
        }
        let mut out = Vec::new();
        walk(doc, doc.root(), &pred, &mut out);
        out[n]
    }

    fn red() -> (u8, u8, u8) {
        (255, 0, 0)
    }

    #[test]
    fn descendant_combinator() {
        let sheet = css::parse(".a .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <div class="a"><div><span class="b">x</span></div></div>
                 <span class="b">y</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let inside = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let outside = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&inside].color, red());
        assert_ne!(map[&outside].color, red());
    }

    #[test]
    fn child_combinator() {
        let sheet = css::parse(".a > .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <div class="a"><span class="b">direct</span></div>
                 <div class="a"><div><span class="b">grand</span></div></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let direct = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let grand = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&direct].color, red());
        assert_ne!(map[&grand].color, red());
    }

    #[test]
    fn adjacent_sibling_combinator() {
        let sheet = css::parse(".a + .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <span class="a">a</span><span class="b">adjacent</span>
                 <span class="x">gap</span><span class="b">notadjacent</span>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let adj = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let notadj = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&adj].color, red());
        assert_ne!(map[&notadj].color, red());
    }

    #[test]
    fn general_sibling_combinator() {
        let sheet = css::parse(".a ~ .b { color: red }");
        let doc = html::parse(
            r#"<html><body>
                 <span class="a">a</span><span class="x">x</span><span class="b">after</span>
                 <div><span class="b">nested-before-no-a</span></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let after = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "b"));
        let nested = elem_nth(&doc, 1, |e| e.classes().any(|c| c == "b"));
        assert_eq!(map[&after].color, red());
        assert_ne!(map[&nested].color, red());
    }

    #[test]
    fn nth_child_and_structural() {
        let sheet = css::parse(
            "li:nth-child(2) { color: red }
             li:first-child { font-weight: bold }
             li:last-child { font-style: italic }
             li:nth-child(odd) { letter-spacing: 3px }",
        );
        let doc =
            html::parse(r#"<html><body><ul><li>1</li><li>2</li><li>3</li></ul></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let li1 = elem_nth(&doc, 0, |e| e.tag == "li");
        let li2 = elem_nth(&doc, 1, |e| e.tag == "li");
        let li3 = elem_nth(&doc, 2, |e| e.tag == "li");
        assert_eq!(map[&li2].color, red()); // nth-child(2)
        assert!(map[&li1].bold); // first-child
        assert!(map[&li3].italic); // last-child
        assert_eq!(map[&li1].letter_spacing, 3.0); // odd → 1
        assert_eq!(map[&li3].letter_spacing, 3.0); // odd → 3
        assert_eq!(map[&li2].letter_spacing, 0.0); // even → not odd
    }

    #[test]
    fn only_child_and_of_type() {
        let sheet = css::parse(
            "p:only-child { color: red }
             span:first-of-type { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div><p>solo</p></div>
                 <div><p>a</p><p>b</p></div>
                 <div><span>s1</span><em>e</em><span>s2</span></div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let solo = elem_nth(&doc, 0, |e| e.tag == "p");
        let paired = elem_nth(&doc, 1, |e| e.tag == "p");
        assert_eq!(map[&solo].color, red());
        assert_ne!(map[&paired].color, red());
        let s1 = elem_nth(&doc, 0, |e| e.tag == "span");
        let s2 = elem_nth(&doc, 1, |e| e.tag == "span");
        assert!(map[&s1].bold);
        assert!(!map[&s2].bold);
    }

    #[test]
    fn attribute_selectors() {
        let sheet = css::parse(
            "[data-x] { color: red }
             input[type=text] { font-weight: bold }
             a[href^=\"https\"] { font-style: italic }
             [class~=foo] { letter-spacing: 2px }
             [type=TEXT i] { text-decoration: underline }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div data-x="1">x</div>
                 <input type="text">
                 <a href="https://example.com">link</a>
                 <a href="http://nope.com">nope</a>
                 <span class="foo bar">word</span>
                 <input type="TEXT">
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let dx = elem_nth(&doc, 0, |e| e.attrs.contains_key("data-x"));
        assert_eq!(map[&dx].color, red());
        let inp = elem_nth(&doc, 0, |e| e.tag == "input");
        assert!(map[&inp].bold);
        let a_https = elem_nth(&doc, 0, |e| e.tag == "a");
        let a_http = elem_nth(&doc, 1, |e| e.tag == "a");
        assert!(map[&a_https].italic);
        assert!(!map[&a_http].italic);
        let foo = elem_nth(&doc, 0, |e| e.classes().any(|c| c == "foo"));
        assert_eq!(map[&foo].letter_spacing, 2.0);
        // case-insensitive [type=TEXT i] matches both lowercase and uppercase type.
        let inp_upper = elem_nth(&doc, 1, |e| e.tag == "input");
        assert!(map[&inp].underline);
        assert!(map[&inp_upper].underline);
    }

    #[test]
    fn state_checked_and_disabled() {
        let sheet = css::parse(
            "input:checked { color: red }
             button:disabled { font-weight: bold }
             input:enabled { font-style: italic }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <input type="checkbox" checked>
                 <input type="checkbox">
                 <button disabled>b</button>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let checked = elem_nth(&doc, 0, |e| e.tag == "input");
        let unchecked = elem_nth(&doc, 1, |e| e.tag == "input");
        assert_eq!(map[&checked].color, red());
        assert_ne!(map[&unchecked].color, red());
        assert!(map[&unchecked].italic); // :enabled (no disabled attr)
        let btn = elem_nth(&doc, 0, |e| e.tag == "button");
        assert!(map[&btn].bold);
    }

    #[test]
    fn hover_and_focus_via_interaction_state() {
        let sheet = css::parse(
            ".btn:hover { color: red }
             .field:focus { font-weight: bold }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <a class="btn"><span>label</span></a>
                 <input class="field">
               </body></html>"#,
        );
        let btn = elem(&doc, |e| e.classes().any(|c| c == "btn"));
        let label = elem(&doc, |e| e.tag == "span");
        let field = elem(&doc, |e| e.classes().any(|c| c == "field"));

        // Hover the inner span: `.btn:hover` should match the ancestor `.btn` too.
        set_interaction_state(Some(label.0), None);
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        assert_eq!(map[&btn].color, red());

        // Focus the field.
        set_interaction_state(None, Some(field.0));
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        assert!(map[&field].bold);

        // Clear state: neither matches.
        set_interaction_state(None, None);
        let map = cascade(&doc, &[sheet]);
        assert_ne!(map[&btn].color, red());
        assert!(!map[&field].bold);
    }

    #[test]
    fn functional_not_is_where() {
        let sheet = css::parse(
            "div:not(.x) { color: red }
             :is(.a, .b) { font-weight: bold }
             :where(.c) { font-style: italic }",
        );
        let doc = html::parse(
            r#"<html><body>
                 <div>plain</div>
                 <div class="x">excluded</div>
                 <div class="a">isa</div>
                 <div class="c">wherec</div>
               </body></html>"#,
        );
        let map = cascade(&doc, &[sheet]);
        let plain = elem_nth(&doc, 0, |e| e.tag == "div");
        let excluded = elem_nth(&doc, 0, |e| e.tag == "div" && e.classes().any(|c| c == "x"));
        let isa = elem(&doc, |e| e.classes().any(|c| c == "a"));
        let wherec = elem(&doc, |e| e.classes().any(|c| c == "c"));
        assert_eq!(map[&plain].color, red()); // :not(.x) matches a plain div
        assert_ne!(map[&excluded].color, red()); // .x excluded
        assert!(map[&isa].bold); // :is(.a, .b)
        assert!(map[&wherec].italic); // :where(.c)
    }

    #[test]
    fn specificity_id_class_type_and_not() {
        // #id beats .cls beats tag.
        assert!(
            compile_selector("#x").unwrap().specificity
                > compile_selector(".c").unwrap().specificity
        );
        assert!(
            compile_selector(".c").unwrap().specificity
                > compile_selector("p").unwrap().specificity
        );
        // :not(#x) carries id-level specificity.
        assert_eq!(
            compile_selector("div:not(#x)").unwrap().specificity,
            compile_selector("#x").unwrap().specificity
                + compile_selector("div").unwrap().specificity
        );
        // :where() contributes ZERO specificity (only the type counts here).
        assert_eq!(
            compile_selector("div:where(.a.b.c)").unwrap().specificity,
            compile_selector("div").unwrap().specificity
        );
    }

    #[test]
    fn where_zero_specificity_loses_to_class() {
        // `:where(.hi)` adds 0 specificity, so a plain `.lo` (class) should win on source order
        // when both target the same element and `.lo` comes later.
        let sheet = css::parse(
            ":where(.hi) { color: blue }
             .lo { color: red }",
        );
        let doc = html::parse(r#"<html><body><p class="hi lo">t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        // Equal specificity (0 vs 10) → .lo (10) wins → red.
        assert_eq!(map[&p].color, red());
    }

    #[test]
    fn pseudo_element_does_not_apply_to_originating_element() {
        // `::before { color: red }` styles the pseudo, NOT the element: `p` itself stays blue.
        // And with no `content`, no pseudo box is generated at all.
        let sheet = css::parse(
            "p::before { color: red }
             p { color: blue }",
        );
        let doc = html::parse(r#"<html><body><p>t</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let p = elem(&doc, |e| e.tag == "p");
        assert_eq!(map[&p].color, (0, 0, 255)); // only `p { blue }` applied to the element
        assert!(map[&p].before.is_none()); // no `content` → no generated box
                                           // The compile step now KEEPS pseudo-elements (routing them to ::before/::after).
        assert_eq!(
            compile_selector("p::before").unwrap().pseudo_element,
            Some(PseudoElement::Before)
        );
        assert_eq!(
            compile_selector("div::after").unwrap().pseudo_element,
            Some(PseudoElement::After)
        );
    }

    #[test]
    fn pseudo_element_before_generates_content() {
        let sheet = css::parse(r#".x::before { content: "→" } p { color: blue }"#);
        let doc = html::parse(r#"<html><body><div class="x">hi</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        let before = map[&d].before.as_ref().expect("::before box");
        assert_eq!(before.content.as_deref(), Some("→"));
        assert!(map[&d].after.is_none());
    }

    #[test]
    fn pseudo_element_empty_and_none_generate_no_or_empty_box() {
        let sheet = css::parse(
            r#"div::after { content: "" }
               span::after { content: none }"#,
        );
        let doc = html::parse(r#"<html><body><div>d</div><span>s</span></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        let s = elem(&doc, |e| e.tag == "span");
        // Empty string → a box with empty content (still Some, so styling could show).
        assert_eq!(map[&d].after.as_ref().unwrap().content.as_deref(), Some(""));
        // `content: none` → no box at all.
        assert!(map[&s].after.is_none());
    }

    #[test]
    fn pseudo_element_content_attr() {
        let sheet = css::parse("div::before { content: attr(data-label) }");
        let doc = html::parse(r#"<html><body><div data-label="Note">x</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(
            map[&d].before.as_ref().unwrap().content.as_deref(),
            Some("Note")
        );
    }

    #[test]
    fn pseudo_element_carries_distinct_paint_style() {
        let sheet = css::parse(
            r#"div { color: rgb(0,0,255) }
               div::before { content: "x"; color: rgb(255,0,0); background-color: rgb(0,255,0) }"#,
        );
        let doc = html::parse(r#"<html><body><div>d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(map[&d].color, (0, 0, 255)); // element stays blue
        let before = map[&d].before.as_ref().unwrap();
        assert_eq!(before.color, (255, 0, 0)); // pseudo is red
        assert_eq!(before.background_color, Some((0, 255, 0)));
    }

    #[test]
    fn pseudo_element_legacy_single_colon() {
        let sheet = css::parse(r#"div:before { content: "L" }"#);
        let doc = html::parse(r#"<html><body><div>d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        assert_eq!(
            map[&d].before.as_ref().unwrap().content.as_deref(),
            Some("L")
        );
    }

    #[test]
    fn pseudo_element_specificity_class_beats_type() {
        let sheet = css::parse(
            r#"div::before { content: "a" }
               .x::before { content: "b" }"#,
        );
        let doc = html::parse(r#"<html><body><div class="x">d</div></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let d = elem(&doc, |e| e.tag == "div");
        // `.x::before` (class) wins over `div::before` (type) → "b".
        assert_eq!(
            map[&d].before.as_ref().unwrap().content.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn parse_gcs_pseudo_normalization() {
        use GcsPseudo::*;
        // No leading colon / empty → use the element.
        assert_eq!(parse_gcs_pseudo(""), Element);
        assert_eq!(parse_gcs_pseudo("before"), Element);
        assert_eq!(parse_gcs_pseudo("totallynotapseudo"), Element);
        // Recognized pseudos (both colon forms for the legacy four).
        assert_eq!(parse_gcs_pseudo("::before"), Pseudo("before".into()));
        assert_eq!(parse_gcs_pseudo(":before"), Pseudo("before".into()));
        assert_eq!(parse_gcs_pseudo("::after"), Pseudo("after".into()));
        assert_eq!(parse_gcs_pseudo("::marker"), Pseudo("marker".into()));
        // Functional pseudos.
        assert_eq!(
            parse_gcs_pseudo("::highlight(name)"),
            Pseudo("highlight(name)".into())
        );
        assert_eq!(
            parse_gcs_pseudo("::highlight( name "),
            Pseudo("highlight(name)".into())
        ); // auto-closed
        assert_eq!(
            parse_gcs_pseudo("::picker(select)"),
            Pseudo("picker(select)".into())
        );
        // CSS escapes resolve.
        assert_eq!(parse_gcs_pseudo(r":bef\oRE"), Pseudo("before".into()));
        // Invalid forms → empty style.
        assert_eq!(parse_gcs_pseudo("::totallynotapseudo"), Invalid);
        assert_eq!(parse_gcs_pseudo(":totallynotapseudo"), Invalid);
        assert_eq!(parse_gcs_pseudo("::before,"), Invalid);
        assert_eq!(parse_gcs_pseudo("::before@after"), Invalid);
        assert_eq!(parse_gcs_pseudo("::marker"), Pseudo("marker".into()));
        assert_eq!(parse_gcs_pseudo(":marker"), Invalid); // needs double colon
        assert_eq!(parse_gcs_pseudo("::highlight(1)"), Invalid); // arg not an ident
        assert_eq!(parse_gcs_pseudo("::highlight()"), Invalid);
        assert_eq!(parse_gcs_pseudo("::picker(div)"), Invalid); // picker only takes `select`
        assert_eq!(parse_gcs_pseudo("::view-transition-group(*)"), Invalid); // `*` not accepted
    }

    #[test]
    fn compute_pseudo_style_cascades_pseudo_values() {
        let sheet = css::parse(
            r#"#x { color: rgb(0, 0, 1) }
               #x::before { color: red; content: "x" }
               #x::highlight(foo) { color: rgb(0, 128, 0) }"#,
        );
        let doc = html::parse(r#"<html><body><div id="x">d</div></body></html>"#);
        let map = cascade(&doc, std::slice::from_ref(&sheet));
        let x = elem(&doc, |e| e.tag == "div");
        let es = &map[&x];
        // ::before: cascaded color + content.
        let before =
            compute_pseudo_style(&doc, std::slice::from_ref(&sheet), x, es, "before").unwrap();
        assert_eq!(before.get_property("color"), "rgb(255, 0, 0)");
        assert_eq!(before.get_property("content"), "\"x\"");
        // ::highlight(foo): a named-highlight rule cascades onto the pseudo.
        let hi = compute_pseudo_style(&doc, std::slice::from_ref(&sheet), x, es, "highlight(foo)")
            .unwrap();
        assert_eq!(hi.get_property("color"), "rgb(0, 128, 0)");
        // A pseudo with no matching rules still yields a (non-empty) style inheriting from the el.
        let marker = compute_pseudo_style(&doc, &[sheet], x, es, "marker").unwrap();
        assert_eq!(marker.get_property("color"), "rgb(0, 0, 1)"); // inherited
        assert!(!marker.property_names().is_empty());
    }

    #[test]
    fn empty_and_root_pseudo() {
        let sheet = css::parse(
            ":root { letter-spacing: 5px }
             p:empty { color: red }",
        );
        let doc = html::parse(r#"<html><body><p></p><p>full</p></body></html>"#);
        let map = cascade(&doc, &[sheet]);
        let html_el = elem(&doc, |e| e.tag == "html");
        assert_eq!(map[&html_el].letter_spacing, 5.0);
        let empty_p = elem_nth(&doc, 0, |e| e.tag == "p");
        let full_p = elem_nth(&doc, 1, |e| e.tag == "p");
        assert_eq!(map[&empty_p].color, red());
        assert_ne!(map[&full_p].color, red());
    }

    /// Cross-check: for a doc + sheet exercising combinators/attrs/pseudos, the indexed match set
    /// equals a brute-force `complex_matches` scan over every rule for every element.
    #[test]
    fn indexed_complex_match_set_equals_bruteforce() {
        let sheet = css::parse(
            ".nav a { color: #010101 }
             .card > .title { color: #020202 }
             li:nth-child(2) { color: #030303 }
             a[target=_blank] { color: #040404 }
             input:checked { color: #050505 }
             div:not(.x) { color: #060606 }
             :is(.a, .b) { color: #070707 }
             .a + .b { color: #080808 }
             .a ~ .c { color: #090909 }
             [data-y] { color: #0a0a0a }",
        );
        let ua = user_agent_stylesheet();
        let author = [sheet];
        let index = SelectorIndex::build(&ua, &author);
        let doc = html::parse(
            r#"<html><body>
                 <nav class="nav"><a target="_blank">l</a></nav>
                 <div class="card"><span class="title">t</span></div>
                 <ul><li>1</li><li>2</li></ul>
                 <input type="checkbox" checked>
                 <div class="x">x</div><div>plain</div>
                 <span class="a">a</span><span class="b">b</span><span class="c">c</span>
                 <p data-y="1">y</p>
               </body></html>"#,
        );
        fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut Vec<dom::NodeId>) {
            if let NodeData::Element(_) = &doc.get(id).data {
                out.push(id);
            }
            for &c in &doc.get(id).children {
                walk(doc, c, out);
            }
        }
        let mut ids = Vec::new();
        walk(&doc, doc.root(), &mut ids);
        for id in ids {
            if let NodeData::Element(el) = &doc.get(id).data {
                // Brute-force: scan every rule directly via complex_matches.
                let brute = naive_matches(&doc, id, &ua, &author);
                let indexed = indexed_matches(&doc, id, el, &index);
                assert_eq!(indexed, brute, "match set diverged for <{}>", el.tag);
            }
        }
    }

    // --- get_property (getComputedStyle string serialization) ----------------------------------

    /// Cascade a doc + sheet and return the computed style for the first element matching `pred`.
    fn cs_of(
        html_src: &str,
        sheet_src: &str,
        pred: impl Fn(&dom::ElementData) -> bool,
    ) -> ComputedStyle {
        let sheet = css::parse(sheet_src);
        let doc = html::parse(html_src);
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, pred);
        map[&id].clone()
    }

    #[test]
    fn get_property_display_block_inline_flex_none() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("display"), "block");
        let cs = cs_of("<html><body><span></span></body></html>", "", |e| {
            e.tag == "span"
        });
        assert_eq!(cs.get_property("display"), "inline");
        let cs = cs_of(
            "<html><body><div class='x'></div></body></html>",
            ".x{display:flex}",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("display"), "flex");
        let cs = cs_of(
            "<html><body><div class='x'></div></body></html>",
            ".x{display:none}",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("display"), "none");
    }

    #[test]
    fn get_property_color_serializes_rgb() {
        let cs = cs_of(
            "<html><body><p style='color:red'>t</p></body></html>",
            "",
            |e| e.tag == "p",
        );
        assert_eq!(cs.get_property("color"), "rgb(255, 0, 0)");
    }

    #[test]
    fn get_property_background_color_transparent_default() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("background-color"), "rgba(0, 0, 0, 0)");
        let cs = cs_of(
            "<html><body><div style='background-color:#00ff00'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("background-color"), "rgb(0, 255, 0)");
    }

    #[test]
    fn get_property_font_size_and_weight() {
        let cs = cs_of(
            "<html><body><p style='font-size:20px;font-weight:bold'>t</p></body></html>",
            "",
            |e| e.tag == "p",
        );
        assert_eq!(cs.get_property("font-size"), "20px");
        assert_eq!(cs.get_property("font-weight"), "700");
        let cs = cs_of("<html><body><p>t</p></body></html>", "", |e| e.tag == "p");
        assert_eq!(cs.get_property("font-weight"), "400");
    }

    #[test]
    fn get_property_position() {
        let cs = cs_of(
            "<html><body><div style='position:absolute'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("position"), "absolute");
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("position"), "static");
    }

    #[test]
    fn get_property_width_height_auto_or_px() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("width"), "auto");
        let cs = cs_of(
            "<html><body><div style='width:100px;height:50px'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("width"), "100px");
        assert_eq!(cs.get_property("height"), "50px");
    }

    #[test]
    fn get_property_margin_longhand_and_shorthand() {
        let cs = cs_of(
            "<html><body><div style='margin:10px 20px 30px 40px'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("margin-top"), "10px");
        assert_eq!(cs.get_property("margin-right"), "20px");
        assert_eq!(cs.get_property("margin-bottom"), "30px");
        assert_eq!(cs.get_property("margin-left"), "40px");
        assert_eq!(cs.get_property("margin"), "10px 20px 30px 40px");
        let cs = cs_of(
            "<html><body><div style='margin:5px'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("margin"), "5px");
    }

    #[test]
    fn get_property_opacity_and_padding() {
        let cs = cs_of(
            "<html><body><div style='opacity:0.5;padding:8px'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("opacity"), "0.5");
        assert_eq!(cs.get_property("padding"), "8px");
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("opacity"), "1");
    }

    #[test]
    fn get_property_flex_container() {
        let cs = cs_of(
            "<html><body><div style='display:flex;justify-content:center;flex-direction:column'></div></body></html>",
            "",
            |e| e.tag == "div",
        );
        assert_eq!(cs.get_property("justify-content"), "center");
        assert_eq!(cs.get_property("flex-direction"), "column");
    }

    #[test]
    fn get_property_untracked_returns_empty() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        // Properties we don't model resolve to the empty string (not their initial value).
        // (`caret-color`/`outline-color` ARE modeled — they resolve to the used color — so they're
        // covered separately, not here.)
        assert_eq!(cs.get_property("transform"), "");
        assert_eq!(cs.get_property("cursor"), "");
        assert_eq!(cs.get_property("--custom-var"), "");
        assert_eq!(cs.get_property("transition"), "");
    }

    #[test]
    fn get_property_is_case_insensitive() {
        let cs = cs_of("<html><body><div></div></body></html>", "", |e| {
            e.tag == "div"
        });
        assert_eq!(cs.get_property("DISPLAY"), "block");
        assert_eq!(cs.get_property("Font-Size"), "16px");
    }

    #[test]
    fn property_names_all_resolve_nonempty() {
        let cs = ComputedStyle::default();
        for name in cs.property_names() {
            assert!(
                !cs.get_property(name).is_empty(),
                "property `{name}` listed in property_names() resolved to empty"
            );
        }
    }

    // ------------------------------------------------------------------------------------------
    // border-collapse / presentational hints
    // ------------------------------------------------------------------------------------------

    #[test]
    fn border_collapse_property_parses() {
        let cs = cs_of(
            r#"<html><body><table style="border-collapse: collapse"></table></body></html>"#,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(cs.border_collapse, BorderCollapse::Collapse);
        assert_eq!(cs.get_property("border-collapse"), "collapse");
    }

    #[test]
    fn border_collapse_inherits_to_cells() {
        let cs = cs_of(
            r#"<html><body><table style="border-collapse: collapse"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.border_collapse, BorderCollapse::Collapse);
    }

    #[test]
    fn pres_table_border_attr_borders_table_and_cells() {
        // <table border="2"> → 2px border on the table AND 1px on each cell.
        let doc = html::parse(
            r#"<html><body><table border="2"><tr><td>x</td></tr></table></body></html>"#,
        );
        let map = cascade(&doc, &[]);
        let table = elem(&doc, |e| e.tag == "table");
        let td = elem(&doc, |e| e.tag == "td");
        assert_eq!(map[&table].border.top, 2.0, "table border attr → 2px");
        assert_eq!(map[&td].border.top, 1.0, "table border attr → 1px on cells");
    }

    #[test]
    fn pres_bgcolor_named_and_hex() {
        let red = cs_of(
            r#"<html><body><table><tr><td bgcolor="red">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(red.background_color, Some((255, 0, 0)));
        let hex = cs_of(
            r##"<html><body><table bgcolor="#00ff00"></table></body></html>"##,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(hex.background_color, Some((0, 255, 0)));
    }

    #[test]
    fn mask_shorthand_extracts_url_and_size() {
        // `mask: url(...) no-repeat center / contain` → url + Contain size, parsing past the rest.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("icon.svg") no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("mask should parse");
        assert_eq!(m.url, "icon.svg");
        assert_eq!(m.size, MaskSize::Contain);
    }

    #[test]
    fn mask_url_resolves_against_stylesheet_base_not_document() {
        // The bug: a relative `url()` in an `@import`'d sheet at `/a/b/sheet.css` must resolve
        // against THAT sheet's URL → `/a/x.svg` (stylesheet-relative), not the document.
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url('../x.svg') no-repeat center / contain }"#,
            "https://site.example/a/b/sheet.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "https://site.example/a/x.svg");
    }

    #[test]
    fn mask_url_resolves_against_stylesheet_dir_for_sibling_subdir() {
        // Mirrors the browserscore bug: sheet at /ui/css/icons.css, url('../icons/w3c.svg')
        // → /ui/icons/w3c.svg (NOT the document-relative /icons/w3c.svg).
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url('../icons/w3c.svg') no-repeat center/contain }"#,
            "https://browserscore.dev/ui/css/icons.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "https://browserscore.dev/ui/icons/w3c.svg");
    }

    #[test]
    fn mask_data_url_passes_through_unchanged() {
        // `data:` masks are self-contained and must never be rewritten against a base.
        let doc = html::parse(r#"<html><body><div class="x"></div></body></html>"#);
        let sheet = css::parse_with_base(
            r#".x { mask: url("data:image/svg+xml,<svg></svg>") }"#,
            "https://site.example/a/b/sheet.css",
        );
        let map = cascade(&doc, &[sheet]);
        let id = elem(&doc, |e| e.tag == "div");
        let m = map[&id].mask_image.clone().expect("mask should parse");
        assert_eq!(m.url, "data:image/svg+xml,<svg></svg>");
    }

    #[test]
    fn mask_url_without_base_is_left_relative_for_engine_fallback() {
        // No base (inline-style / base-less sheet): the cascade leaves the url relative; the engine
        // resolves it against the document URL.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("icon.svg") no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        assert_eq!(cs.mask_image.expect("mask").url, "icon.svg");
    }

    #[test]
    fn webkit_mask_is_an_alias() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { -webkit-mask: url(a.svg) center / cover }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("-webkit-mask should parse");
        assert_eq!(m.url, "a.svg");
        assert_eq!(m.size, MaskSize::Cover);
    }

    #[test]
    fn mask_url_resolves_var() {
        // The icon url is behind a custom property (the browserscore pattern).
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { --icon: url(glyph.svg); mask: var(--icon) no-repeat center / contain }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("var()-indirected mask should resolve");
        assert_eq!(m.url, "glyph.svg");
    }

    #[test]
    fn mask_data_url_preserved() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url("data:image/svg+xml,<svg></svg>") }"#,
            |e| e.tag == "div",
        );
        let m = cs.mask_image.expect("data: mask should parse");
        assert!(m.url.starts_with("data:image/svg+xml,"));
        assert_eq!(
            m.size,
            MaskSize::Stretch,
            "no size keyword → Stretch (fit-to-box)"
        );
    }

    #[test]
    fn mask_none_clears() {
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            r#".x { mask: url(a.svg); mask: none }"#,
            |e| e.tag == "div",
        );
        assert!(cs.mask_image.is_none(), "mask: none clears the mask");
    }

    #[test]
    fn pres_align_center_on_cell() {
        let cs = cs_of(
            r#"<html><body><table><tr><td align="center">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.text_align, TextAlign::Center);
    }

    #[test]
    fn pres_cellpadding_and_cellspacing() {
        let td = cs_of(
            r#"<html><body><table cellpadding="10" cellspacing="4"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(td.padding.top, 10.0, "cellpadding → cell padding");
        let table = cs_of(
            r#"<html><body><table cellpadding="10" cellspacing="4"><tr><td>x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "table",
        );
        assert_eq!(table.border_spacing, 4.0, "cellspacing → border-spacing");
    }

    #[test]
    fn pres_width_attr_on_cell() {
        let cs = cs_of(
            r#"<html><body><table><tr><td width="200">x</td></tr></table></body></html>"#,
            "",
            |e| e.tag == "td",
        );
        assert_eq!(cs.width, Some(200.0));
    }

    #[test]
    fn author_css_overrides_presentational_hint() {
        // bgcolor="red" but author CSS sets blue → CSS wins (hints are lowest precedence).
        let cs = cs_of(
            r#"<html><body><table><tr><td bgcolor="red">x</td></tr></table></body></html>"#,
            "td { background-color: blue }",
            |e| e.tag == "td",
        );
        assert_eq!(
            cs.background_color,
            Some((0, 0, 255)),
            "author CSS should beat bgcolor attr"
        );
    }

    // ------------------------------------------------------------------------------------------
    // CSSOM resolved insets / !important / value retention
    // ------------------------------------------------------------------------------------------

    #[test]
    fn static_inset_resolves_to_computed_value() {
        // `position: static`: the inset *resolved value* is the computed value — `auto` stays `auto`,
        // percentages stay percentages, lengths absolutize to px.
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: static; top: auto; left: 10%; bottom: 1em; font-size: 10px }",
            |e| e.tag == "div",
        );
        assert_eq!(cs.resolved_inset(EdgeSide::Top, false, f32::NAN), "auto");
        assert_eq!(cs.resolved_inset(EdgeSide::Left, false, f32::NAN), "10%");
        assert_eq!(cs.resolved_inset(EdgeSide::Bottom, false, f32::NAN), "10px");
        // 1em @ 10px
    }

    #[test]
    fn relative_inset_resolves_percentage_and_auto_pair() {
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: relative; top: 10%; bottom: auto }",
            |e| e.tag == "div",
        );
        // 10% of a 100px containing block → 10px; the auto bottom mirrors the negated top.
        assert_eq!(cs.resolved_inset(EdgeSide::Top, false, 100.0), "10px");
        assert_eq!(cs.resolved_inset(EdgeSide::Bottom, false, 100.0), "-10px");
    }

    #[test]
    fn nobox_inset_preserves_computed_value() {
        let cs = cs_of(
            "<html><body><div></div></body></html>",
            "div { position: absolute; left: 25% }",
            |e| e.tag == "div",
        );
        // Box-less (display:none): even an absolutely-positioned element reports the computed value.
        assert_eq!(cs.resolved_inset(EdgeSide::Left, true, 400.0), "25%");
    }

    #[test]
    fn important_declaration_wins_over_higher_specificity() {
        // `div` (low specificity) with `!important` beats `.x` (higher specificity) without it.
        let cs = cs_of(
            r#"<html><body><div class="x"></div></body></html>"#,
            "div { color: blue !important } .x { color: red }",
            |e| e.tag == "div",
        );
        assert_eq!(cs.color, (0, 0, 255), "!important should win the cascade");
        // And the value parses despite the trailing `!important`.
        assert_eq!(cs.get_property("color"), "rgb(0, 0, 255)");
    }

    #[test]
    fn split_importance_strips_keyword() {
        assert_eq!(split_importance("red !important"), ("red", true));
        assert_eq!(
            split_importance("rgb(0, 0, 255)!important"),
            ("rgb(0, 0, 255)", true)
        );
        assert_eq!(split_importance("10px"), ("10px", false));
    }

    #[test]
    fn parse_inset_value_retains_percent_and_calc() {
        assert_eq!(parse_inset_value("auto", 16.0), InsetValue::Auto);
        assert_eq!(parse_inset_value("10%", 16.0), InsetValue::Percent(10.0));
        assert_eq!(parse_inset_value("1em", 10.0), InsetValue::Length(10.0));
        assert_eq!(
            parse_inset_value("calc(10% - 1px)", 16.0),
            InsetValue::Calc {
                pct: 10.0,
                px: -1.0
            }
        );
    }
}
