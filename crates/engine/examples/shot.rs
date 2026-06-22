//! Headless: load a URL, render to a PNG, print console errors.
//! Usage: cargo run -p engine --example shot -- <url> <out.png> [width] [height]

fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: shot <url> <out.png> [w] [h]");
    let out = std::env::args().nth(2).unwrap_or_else(|| "shot.png".into());
    let w: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1280);
    let h: u32 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

    let mut engine = engine::Engine::new();
    engine.set_viewport(w, h, 1.0);
    let code = engine.load_url(&url);
    eprintln!("load_url -> {code}");

    for line in engine.console_lines() {
        eprintln!("console: {line}");
    }

    let fb = engine.render();
    let (fw, fh, stride) = (fb.width, fb.height, fb.stride);
    eprintln!("framebuffer {fw}x{fh} stride {stride}");
    let mut img = image::RgbaImage::new(fw.max(1), fh.max(1));
    for y in 0..fh {
        for x in 0..fw {
            let i = (y * stride + x * 4) as usize;
            if i + 3 < fb.pixels.len() {
                img.put_pixel(
                    x,
                    y,
                    image::Rgba([
                        fb.pixels[i],
                        fb.pixels[i + 1],
                        fb.pixels[i + 2],
                        fb.pixels[i + 3],
                    ]),
                );
            }
        }
    }
    image::DynamicImage::ImageRgba8(img)
        .save(&out)
        .expect("save png");
    eprintln!("wrote {out}");
}
