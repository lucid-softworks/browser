//! Break opportunities *inside* a text run (a UAX #14 subset for CJK).
//!
//! Inline layout treats the text between breaking spaces as one atomic unit, so a line can only
//! ever be broken where the author wrote a space. That is fine for Latin prose but wrong for
//! scripts that don't use spaces: a Japanese or Chinese paragraph is a single unbreakable "word",
//! so it never wraps, overflows its container, and lays out one line tall instead of many.
//!
//! This module answers "may a line break between these two characters?". The rules implemented are
//! the ones that matter for real text, from UAX #14:
//!
//! * a break is allowed between two ideographic characters (LB18/LB999),
//! * never *before* a closing bracket, sentence-ending mark, or non-starter such as a small kana
//!   or a prolonged sound mark (LB13/LB16/LB19) — those may not begin a line,
//! * never *after* an opening bracket (LB14) — it may not end a line.
//!
//! `word-break` selects between three answers: `normal` gives the ideograph-to-ideograph
//! opportunities above, `break-all` adds one between every pair including Latin, and `keep-all`
//! removes the letter-to-letter ones so a run wraps only at spaces. The pair prohibitions apply to
//! all three — no value of `word-break` may strand a full stop at the start of a line.
//!
//! Hangul is deliberately excluded from the ideographic set: Korean is written with spaces and
//! UAX #14 only permits breaking between syllables under an explicit `line-break` value, so
//! treating it like Han here would wrap Korean mid-word.

/// Characters we apply ideographic breaking to: Han, kana, CJK punctuation and the fullwidth
/// forms. Deliberately excludes Hangul (see the module note).
fn is_ideographic(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2EFF    // CJK radicals supplement
        | 0x3000..=0x303F  // CJK symbols and punctuation
        | 0x3040..=0x309F  // hiragana
        | 0x30A0..=0x30FF  // katakana
        | 0x31F0..=0x31FF  // katakana phonetic extensions
        | 0x3400..=0x4DBF  // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF  // CJK unified ideographs
        | 0xF900..=0xFAFF  // CJK compatibility ideographs
        | 0xFF00..=0xFFEF  // halfwidth and fullwidth forms
        | 0x20000..=0x2FA1F // extensions B onwards
    )
}

/// Classes that may not *begin* a line: closing brackets (CL/CP), sentence-ending and separating
/// marks (EX/IS), and non-starters (NS) such as small kana and the prolonged sound mark.
fn forbidden_at_line_start(c: char) -> bool {
    matches!(
        c,
        // Closing brackets, CJK and fullwidth.
        '）' | '〕' | '］' | '｝' | '〉' | '》' | '」' | '』' | '】' | '〗' | '〙' | '〛'
        | '｠' | '〞' | '’' | '”'
        // Sentence-ending and separating marks.
        | '、' | '。' | '，' | '．' | '：' | '；' | '！' | '？' | '‼' | '⁇' | '⁈' | '⁉'
        | '・' | '･' | '｡' | '､'
        // Non-starters: prolonged sound mark, iteration marks, small kana.
        | 'ー' | 'ｰ' | 'ゝ' | 'ゞ' | '々' | '〻' | '〳' | '〴' | '〵'
        | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'っ' | 'ゃ' | 'ゅ' | 'ょ' | 'ゎ' | 'ゕ' | 'ゖ'
        | 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ッ' | 'ャ' | 'ュ' | 'ョ' | 'ヮ' | 'ヵ' | 'ヶ'
        | 'ﾞ' | 'ﾟ'
    )
}

/// Opening brackets (OP), which may not *end* a line.
fn forbidden_at_line_end(c: char) -> bool {
    matches!(
        c,
        '（' | '〔'
            | '［'
            | '｛'
            | '〈'
            | '《'
            | '「'
            | '『'
            | '【'
            | '〖'
            | '〘'
            | '〚'
            | '｟'
            | '〝'
            | '‘'
            | '“'
    )
}

/// Whether a line may break between `prev` and `next` under `mode`.
///
/// The pair prohibitions are checked first because they hold regardless of `word-break`: even
/// `break-all` must not strand a closing bracket or a full stop at the start of a line.
fn can_break_between(prev: char, next: char, mode: style::WordBreak) -> bool {
    if forbidden_at_line_start(next) || forbidden_at_line_end(prev) {
        return false;
    }
    // U+3000 IDEOGRAPHIC SPACE is a space, not a typographic letter unit, despite sitting in the CJK
    // punctuation block. `keep-all` only suppresses opportunities *between letters*, so the one after
    // U+3000 survives it — and it is the sole thing that lets `keep-all` text wrap at all.
    if prev == '\u{3000}' {
        return true;
    }
    match mode {
        style::WordBreak::KeepAll => false,
        style::WordBreak::BreakAll => true,
        style::WordBreak::Normal => is_ideographic(prev) && is_ideographic(next),
    }
}

/// Cheap pre-check so the common all-Latin run does no segmentation work at all.
fn has_ideographic(s: &str) -> bool {
    s.chars().any(is_ideographic)
}

/// Split a run into the largest pieces that must stay together on one line.
///
/// Returns one segment when there is no interior break opportunity, so callers can treat the
/// result uniformly. Only the first segment inherits the run's preceding space; the rest follow
/// with no inter-segment space, since an ideographic break inserts nothing.
pub(crate) fn segments(run: &str, mode: style::WordBreak) -> Vec<&str> {
    // Skip the scan when the run provably has no interior opportunity, so the overwhelmingly common
    // all-Latin `normal` run costs one cheap pass and no allocation beyond the single-element vec.
    let may_have_break = match mode {
        style::WordBreak::BreakAll => true,
        style::WordBreak::Normal => has_ideographic(run),
        // `keep-all` suppresses every letter-to-letter opportunity, leaving only the one after an
        // ideographic space.
        style::WordBreak::KeepAll => run.contains('\u{3000}'),
    };
    if !may_have_break {
        return vec![run];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    for (i, c) in run.char_indices() {
        if let Some(p) = prev {
            if can_break_between(p, c, mode) {
                out.push(&run[start..i]);
                start = i;
            }
        }
        prev = Some(c);
    }
    out.push(&run[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `word-break: normal`, the default every test used before the property was honoured.
    fn seg(run: &str) -> Vec<&str> {
        segments(run, style::WordBreak::Normal)
    }

    #[test]
    fn latin_runs_are_never_segmented() {
        assert_eq!(seg("hello"), vec!["hello"]);
        assert_eq!(seg(""), vec![""]);
    }

    #[test]
    fn ideographs_break_between_every_character() {
        // Without this a CJK paragraph is one atomic word and never wraps.
        assert_eq!(seg("漢字"), vec!["漢", "字"]);
        assert_eq!(seg("テスト"), vec!["テ", "ス", "ト"]);
    }

    #[test]
    fn closing_marks_do_not_start_a_line() {
        // A break before 」 or 。 would leave the mark stranded at the start of the next line.
        assert_eq!(seg("「漢字」"), vec!["「漢", "字」"]);
        assert_eq!(seg("漢。"), vec!["漢。"]);
        assert_eq!(seg("あっ"), vec!["あっ"]);
    }

    #[test]
    fn opening_brackets_do_not_end_a_line() {
        assert_eq!(seg("字（漢"), vec!["字", "（漢"]);
    }

    #[test]
    fn hangul_is_left_alone() {
        // Korean is space-separated; breaking between syllables needs an explicit word-break.
        assert_eq!(seg("한국어"), vec!["한국어"]);
    }

    #[test]
    fn mixed_latin_and_ideographs_keeps_latin_intact() {
        assert_eq!(seg("ab漢字"), vec!["ab漢", "字"]);
    }

    #[test]
    fn keep_all_makes_a_cjk_run_atomic() {
        // The whole point of `keep-all`: the run gets no interior opportunities, so it wraps only at
        // spaces. Without this the default breaking would split it and the declaration would be a
        // no-op.
        assert_eq!(segments("漢字", style::WordBreak::KeepAll), vec!["漢字"]);
        assert_eq!(
            segments("テスト", style::WordBreak::KeepAll),
            vec!["テスト"]
        );
    }

    #[test]
    fn keep_all_still_breaks_after_ideographic_space() {
        // WPT `word-break-keep-all-005`: U+3000 is a space, so `keep-all` must not suppress the
        // opportunity after it — otherwise the run cannot wrap anywhere and overflows instead.
        assert_eq!(
            segments("字字\u{3000}字字", style::WordBreak::KeepAll),
            vec!["字字\u{3000}", "字字"]
        );
    }

    #[test]
    fn break_all_segments_latin_too() {
        assert_eq!(
            segments("hello", style::WordBreak::BreakAll),
            vec!["h", "e", "l", "l", "o"]
        );
    }

    #[test]
    fn break_all_still_honours_pair_prohibitions() {
        // Breaking "anywhere" must not strand a closing mark at the start of a line.
        assert_eq!(
            segments("漢。", style::WordBreak::BreakAll),
            vec!["漢。"],
            "。 must not be pushed to the next line",
        );
        assert_eq!(
            segments("a（b", style::WordBreak::BreakAll),
            vec!["a", "（b"],
            "（ must not be left at the end of a line",
        );
    }
}
