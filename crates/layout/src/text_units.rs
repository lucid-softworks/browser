//! Typographic unit boundary adapter for eventual native implementation.
use unicode_segmentation::UnicodeSegmentation;
pub(crate) fn grapheme_indices(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.grapheme_indices(true)
}
