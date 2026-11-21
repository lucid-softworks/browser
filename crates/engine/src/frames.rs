//! Child documents use the same layout and paint pipeline as the top-level page. Each child is
//! rasterized to its viewport, then composited as replaced content at the iframe's stacking slot.
use crate::*;
use std::hash::{Hash, Hasher};

pub(crate) struct FrameRenderer {
    generation: u64,
    version: Option<u64>,
    resource_key: u64,
    engine: Box<Engine>,
}

fn image_key(doc: &dom::Document) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for i in 0..doc.len() {
        if let dom::NodeData::Element(el) = &doc.get(dom::NodeId(i)).data {
            if matches!(el.tag.as_str(), "img" | "object" | "embed") {
                i.hash(&mut hash);
                el.attrs.get("src").hash(&mut hash);
                el.attrs.get("data").hash(&mut hash);
            }
        }
    }
    hash.finish()
}

impl Engine {
    fn frame_pixels(&self, out: &mut Vec<js::FramePixels>) {
        for frame in self.frame_renderers.values() {
            out.push(js::FramePixels {
                generation: frame.generation,
                canvases: frame
                    .engine
                    .canvas_bitmaps
                    .iter()
                    .map(|(id, image)| (id.0, image.w, image.h, image.rgba.clone()))
                    .collect(),
            });
            frame.engine.frame_pixels(out);
        }
    }

    pub(crate) fn has_frame_content(&self) -> bool {
        !self.frame_renderers.is_empty()
            || matches!(&self.state, LoadState::Loaded { doc: Some(doc), .. }
            if (0..doc.len()).any(|i| matches!(&doc.get(dom::NodeId(i)).data, dom::NodeData::Element(el) if el.tag == "iframe")))
    }

    pub(crate) fn update_frame_bitmaps(&mut self) {
        if !self.has_frame_content() && self.frame_snapshots.is_none() {
            return;
        }
        let snapshots = if let Some(session) = &self.session {
            session.frame_snapshots()
        } else {
            self.frame_snapshots.take().unwrap_or_default()
        };
        let live: std::collections::HashSet<_> =
            snapshots.iter().map(|s| dom::NodeId(s.node_id)).collect();
        if let LoadState::Loaded { images, .. } = &mut self.state {
            for id in self.frame_renderers.keys() {
                images.remove(id);
            }
        }
        self.frame_renderers.retain(|id, _| live.contains(id));
        for snapshot in snapshots {
            let id = dom::NodeId(snapshot.node_id);
            let font = self.font.clone();
            // Match the raster-image allocation bound; CSS can request unbounded iframe sizes.
            let pixels = (snapshot.width as f64 * self.scale as f64)
                * (snapshot.height as f64 * self.scale as f64);
            if pixels > MAX_IMAGE_PIXELS as f64 {
                continue;
            }
            let renderer = self
                .frame_renderers
                .entry(id)
                .or_insert_with(|| FrameRenderer {
                    generation: snapshot.generation,
                    version: None,
                    resource_key: 0,
                    engine: Box::new(Engine::with_font(font.clone())),
                });
            if renderer.generation != snapshot.generation {
                *renderer = FrameRenderer {
                    generation: snapshot.generation,
                    version: None,
                    resource_key: 0,
                    engine: Box::new(Engine::with_font(font.clone())),
                };
            }
            let child = &mut renderer.engine;
            child.js_context = snapshot.host;
            if child.is_dark != self.is_dark {
                child.layout_cache = None;
            }
            child.is_dark = self.is_dark;
            child.set_viewport(snapshot.width, snapshot.height, self.scale);
            if renderer.version != Some(snapshot.version) {
                let key = image_key(&snapshot.doc);
                let images = if renderer.version.is_none() || key != renderer.resource_key {
                    collect_images(&snapshot.doc, &snapshot.url, &mut Vec::new())
                } else if let LoadState::Loaded { images, .. } = &mut child.state {
                    std::mem::take(images)
                } else {
                    HashMap::new()
                };
                renderer.resource_key = key;
                child.load_web_fonts(&snapshot.styles, &snapshot.url);
                child.state = LoadState::Loaded {
                    url: snapshot.url,
                    doc: Some(snapshot.doc),
                    styles: snapshot.styles,
                    console: Vec::new(),
                    images,
                };
                child.layout_cache = None;
                renderer.version = Some(snapshot.version);
            }
            child.scroll_y = snapshot.scroll_y * self.scale;
            child.frame_canvas_lists = Some(snapshot.canvas_lists);
            child.frame_scroll_offsets = Some(snapshot.element_scroll_offsets);
            child.frame_snapshots = Some(snapshot.children);
            let frame = child.render();
            let bitmap = DecodedImage {
                w: frame.width,
                h: frame.height,
                rgba: frame.pixels.clone(),
            };
            if let LoadState::Loaded { images, .. } = &mut self.state {
                images.insert(id, bitmap);
            }
        }
        if let Some(session) = &self.session {
            let mut pixels = Vec::new();
            self.frame_pixels(&mut pixels);
            if !pixels.is_empty() {
                session.set_frame_pixels(pixels);
            }
        }
        style::set_viewport_metrics(self.vp_w as f32, self.vp_h as f32, self.scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn escape(value: &str) -> String {
        value.replace('&', "&amp;").replace('"', "&quot;")
    }
    fn page(html: &str) -> Engine {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "browser-frame-{}-{}.html",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, html).unwrap();
        let mut engine = Engine::new();
        engine.set_viewport(200, 160, 1.0);
        assert_eq!(engine.load_url(&format!("file://{}", path.display())), 0);
        std::fs::remove_file(path).unwrap();
        engine
    }
    fn pixel(fb: &Framebuffer, x: usize, y: usize) -> [u8; 3] {
        let i = y * fb.stride as usize + x * 4;
        [fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]]
    }
    #[test]
    fn iframe_paints_live_content_clipped_in_parent_stacking_order() {
        let child = "<body style='margin:0;background:red'><div style='width:300px;height:300px;background:red'></div></body>";
        let mut e = page(&format!("<body style='margin:0;background:blue'><iframe id=f style='position:absolute;left:10px;top:10px;width:80px;height:60px;border:4px solid green' srcdoc=\"{}\"></iframe><div style='position:absolute;left:30px;top:30px;width:10px;height:10px;background:lime;z-index:1'></div></body>", escape(child)));
        let fb = e.render();
        assert_eq!(pixel(fb, 15, 15), [255, 0, 0]);
        assert_eq!(pixel(fb, 11, 11), [0, 128, 0]);
        assert_eq!(pixel(fb, 32, 32), [0, 255, 0]);
        assert_eq!(pixel(fb, 100, 40), [0, 0, 255]);
        e.console_eval("f.contentDocument.body.style.background='yellow'; f.contentDocument.body.firstChild.style.background='yellow'");
        assert_eq!(pixel(e.render(), 15, 15), [255, 255, 0]);
        e.console_eval("f.remove()");
        assert_eq!(pixel(e.render(), 15, 15), [0, 0, 255]);
        assert!(e.frame_renderers.is_empty());
    }
    #[test]
    fn iframe_resizes_navigates_and_renders_nested_documents() {
        let inner = "<body style='margin:0;background:lime'></body>";
        let child = format!("<body style='margin:0;background:red'><iframe style='width:30px;height:20px;border:0' srcdoc=\"{}\"></iframe></body>", escape(inner));
        let mut e = page(&format!("<body style='margin:0'><iframe id=f style='width:80px;height:60px;border:0' srcdoc=\"{}\"></iframe></body>",escape(&child)));
        assert_eq!(pixel(e.render(), 10, 10), [0, 255, 0]);
        assert_eq!(pixel(e.render(), 50, 40), [255, 0, 0]);
        e.console_eval("f.style.width='120px'; f.style.height='90px'");
        e.render();
        assert_eq!(
            e.console_eval("f.contentWindow.innerWidth + 'x' + f.contentWindow.innerHeight"),
            "120x90"
        );
        e.console_eval("f.srcdoc=\"<body style='margin:0;background:yellow'></body>\"");
        assert_eq!(pixel(e.render(), 10, 10), [255, 255, 0]);
        assert!(e
            .frame_renderers
            .values()
            .next()
            .unwrap()
            .engine
            .frame_renderers
            .is_empty());
    }
    #[test]
    fn iframe_canvas_is_rasterized_at_device_scale() {
        let child = "<body style='margin:0'><canvas id=c width=40 height=30></canvas><script>var ctx=c.getContext('2d');ctx.fillStyle='red';ctx.fillRect(0,0,40,30)</script></body>";
        let mut e = page(&format!("<body style='margin:0'><iframe id=f style='width:80px;height:60px;border:0' srcdoc=\"{}\"></iframe></body>",escape(child)));
        e.set_viewport(200, 160, 2.0);
        assert_eq!(pixel(e.render(), 50, 30), [255, 0, 0]);
        assert_eq!(pixel(e.render(), 100, 30), [255, 255, 255]);
        assert_eq!(e.console_eval("f.contentWindow.devicePixelRatio"), "2");
        assert_eq!(
            e.console_eval("Array.from(f.contentWindow.ctx.getImageData(1,1,1,1).data).join(',')"),
            "255,0,0,255"
        );
    }
    #[test]
    fn iframe_tasks_have_paint_opportunities_between_them() {
        let mut e = page(
            r#"<body style='margin:0'><iframe id=f style='width:80px;height:60px;border:0' srcdoc="<body style='margin:0;background:white'></body>"></iframe></body>"#,
        );
        e.console_eval("setTimeout(function(){f.contentDocument.body.style.background='red';setTimeout(function(){f.contentDocument.body.style.background='blue'},0)},0)");
        assert_eq!(pixel(e.framebuffer.as_ref().unwrap(), 10, 10), [255, 0, 0]);
        e.tick();
        assert_eq!(pixel(e.framebuffer.as_ref().unwrap(), 10, 10), [0, 0, 255]);
    }
    #[test]
    fn cross_origin_iframe_paints_without_exposing_its_dom() {
        let mut e = page(
            r#"<body style='margin:0'><iframe id=f style='width:80px;height:60px;border:0' src="data:text/html,%3Cbody%20style='margin:0;background:lime'%3E%3C/body%3E"></iframe></body>"#,
        );
        assert_eq!(pixel(e.render(), 10, 10), [0, 255, 0]);
        assert_eq!(e.console_eval("f.contentDocument === null"), "true");
    }
    #[test]
    fn iframe_document_scroll_uses_its_own_viewport() {
        let child = "<body style='margin:0'><div style='height:60px;background:red'></div><div style='height:100px;background:blue'></div></body>";
        let mut e = page(&format!(
            r#"<body style='margin:0;background:lime'><iframe id=f style='width:80px;height:50px;border:0' srcdoc="{}"></iframe></body>"#,
            escape(child)
        ));
        assert_eq!(pixel(e.render(), 10, 10), [255, 0, 0]);
        e.console_eval("f.contentWindow.scrollTo(0,60)");
        assert_eq!(pixel(e.render(), 10, 10), [0, 0, 255]);
        assert_eq!(pixel(e.render(), 100, 10), [0, 255, 0]);
    }
    #[test]
    fn iframe_element_scroll_tracks_reset_detach_and_reattach() {
        let child = "<body style='margin:0'><div id=s style='height:40px;width:60px;overflow:auto'><div style='height:50px;background:red'></div><div style='height:100px;background:blue'></div></div></body>";
        let mut e = page(&format!(
            r#"<body style='margin:0'><iframe id=f style='width:80px;height:60px;border:0' srcdoc="{}"></iframe></body>"#,
            escape(child)
        ));
        assert_eq!(pixel(e.render(), 10, 10), [255, 0, 0]);
        e.console_eval("var s=f.contentDocument.getElementById('s'); s.scrollTop=50");
        assert_eq!(pixel(e.render(), 10, 10), [0, 0, 255]);
        e.console_eval("s.scrollTop=0");
        assert_eq!(pixel(e.render(), 10, 10), [255, 0, 0]);
        e.console_eval("s.scrollTop=50; s.remove()");
        assert_eq!(pixel(e.render(), 10, 10), [255, 255, 255]);
        e.console_eval("f.contentDocument.body.appendChild(s)");
        assert_eq!(pixel(e.render(), 10, 10), [0, 0, 255]);
    }
    #[test]
    fn iframe_percentage_width_does_not_change_auto_height() {
        let mut e = page(
            r#"<body style='margin:0'><div id=p style='width:120px'><iframe id=f style='width:100%;border:0' srcdoc="<body style='margin:0;background:lime'></body>"></iframe></div></body>"#,
        );
        e.render();
        assert_eq!(
            e.console_eval("f.contentWindow.innerWidth + 'x' + f.contentWindow.innerHeight"),
            "120x150"
        );
        e.console_eval("p.style.width='180px'");
        assert_eq!(
            e.console_eval("f.contentWindow.innerWidth + 'x' + f.contentWindow.innerHeight"),
            "180x150"
        );
        assert_eq!(pixel(e.render(), 170, 10), [0, 255, 0]);
    }
}
