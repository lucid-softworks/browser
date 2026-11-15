//! Resources supplied by the desktop shell rather than discovered by the shared engine.

/// A known default page font, available independently of the operating system's font paths.
/// Its SIL Open Font License is distributed beside the source asset.
pub fn bundled_font() -> &'static [u8] {
    include_bytes!("../assets/Inter-Regular.ttf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_is_accepted_and_invalid_replacement_preserves_rendering() {
        let mut engine = engine::Engine::new();
        assert!(engine.set_font_data(bundled_font().to_vec()));
        engine.set_viewport(320, 200, 1.0);
        assert_eq!(engine.load_url("about:blank"), 0);
        let blank = engine.render().pixels.clone();
        engine.console_eval("document.body.textContent = 'Portable browser text';");
        let before = engine.render().pixels.clone();
        assert_ne!(before, blank, "the bundled font paints page text");
        assert!(!engine.set_font_data(vec![0, 1, 2, 3]));
        assert_eq!(engine.render().pixels, before);
        assert!(engine.set_font_data(bundled_font().to_vec()));
        assert!(
            engine.framebuffer().is_none(),
            "valid replacement invalidates pixels"
        );
        assert_eq!(engine.render().pixels, before);
    }
}
