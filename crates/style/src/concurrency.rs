use crate::*;
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_documents_keep_independent_cascade_contexts() {
    let ready = Arc::new(Barrier::new(2));
    let threads: Vec<_> = [false, true]
        .into_iter()
        .map(|dark| {
            let ready = ready.clone();
            std::thread::spawn(move || {
                let width = if dark { 960.0 } else { 320.0 };
                let height = if dark { 720.0 } else { 240.0 };
                let dpr = if dark { 2.0 } else { 1.0 };
                let root_size = if dark { 20.0 } else { 10.0 };
                let base = if dark {
                    "https://dark.example/document/"
                } else {
                    "https://light.example/document/"
                };
                let doc = html::parse("<html><body><p>text</p></body></html>");
                let p = (0..doc.len())
                    .map(dom::NodeId)
                    .find(|&id| matches!(&doc.get(id).data, dom::NodeData::Element(e) if e.tag == "p"))
                    .unwrap();
                let hovered = (!dark).then_some(p.0);
                let focused = dark.then_some(p.0);
                let sheet = css::parse(&format!(
                    ":root {{ color-scheme: light dark; font-size: {root_size}px }}
                     p {{ width: 50vw; height: 2rem; color: red }}
                     p:hover {{ background-color: red }}
                     p:focus {{ background-color: blue }}
                     @media (min-width: 700px) {{ p {{ color: green }} }}"
                ));
                set_viewport_metrics(width, height, dpr);
                set_color_scheme_dark(dark);
                set_interaction_state(hovered, focused);
                set_document_base_url(Some(base));
                let (styles, used_dark) =
                    cascade_with_root_scheme(&doc, std::slice::from_ref(&sheet), dark);
                set_forced_colors(dark);

                // Both documents have installed their context and completed a cascade. A
                // process-global context cannot retain both sets of values across this barrier.
                ready.wait();
                assert_eq!(viewport_metrics(), (width, height, dpr));
                assert_eq!(color_scheme_dark(), dark);
                assert_eq!(root_used_scheme_dark(), dark);
                assert_eq!(used_dark, dark);
                assert_eq!(root_em(), root_size);
                assert_eq!(interaction_hovered(), hovered);
                assert_eq!(interaction_focused(), focused);
                assert_eq!(FORCED_COLORS.get(), dark);
                DOCUMENT_BASE_URL.with(|value| assert_eq!(value.borrow().as_deref(), Some(base)));

                set_forced_colors(false);
                let again = cascade(&doc, &[sheet]);
                for map in [&styles, &again] {
                    let style = &map[&p];
                    assert_eq!(style.width, Some(width * 0.5));
                    assert_eq!(style.height, Some(root_size * 2.0));
                    assert_eq!(style.color, if dark { (0, 128, 0) } else { (255, 0, 0) });
                    assert_eq!(
                        style.background_color,
                        Some(if dark { (0, 0, 255) } else { (255, 0, 0) })
                    );
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}
