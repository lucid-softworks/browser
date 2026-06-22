/// One placed float's margin-box rectangle, recorded in the coordinate space of the block
/// formatting context that owns it.
#[derive(Clone, Copy, Debug)]
struct FloatRect {
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

/// Tracks the floats placed within a single block formatting context so later floats pack beside
/// earlier ones (wrapping down when a row is full) and in-flow content can be narrowed to the gap
/// the floats leave. Coordinates are absolute (the same space layout boxes use).
///
/// This is a deliberately self-contained model: each block container that lays out its children
/// owns its own context, so floats stay within their parent rather than escaping to an ancestor
/// formatting context. That covers the dominant authoring pattern — self-contained float grids and
/// "float beside text" inside a clearfix/`inline-block`/`overflow` wrapper — without the
/// complexity of cross-container float propagation.
#[derive(Default)]
pub(crate) struct FloatCtx {
    /// Left content edge of the formatting context (floats can't go left of this).
    left_edge: f32,
    /// Right content edge of the formatting context (floats can't go right of this).
    right_edge: f32,
    lefts: Vec<FloatRect>,
    rights: Vec<FloatRect>,
}

impl FloatCtx {
    pub(crate) fn new(left_edge: f32, right_edge: f32) -> Self {
        FloatCtx {
            left_edge,
            right_edge,
            lefts: Vec::new(),
            rights: Vec::new(),
        }
    }

    /// True if no floats have been placed (the common case — lets callers skip all float work).
    pub(crate) fn is_empty(&self) -> bool {
        self.lefts.is_empty() && self.rights.is_empty()
    }

    /// The available horizontal band `[left, right]` for content occupying the vertical range
    /// `[y, y + height)`, after subtracting every float that overlaps that range.
    fn band(&self, y: f32, height: f32) -> (f32, f32) {
        let y0 = y;
        let y1 = y + height.max(0.0);
        let overlaps = |f: &FloatRect| f.top < y1 && f.bottom > y0;
        let mut left = self.left_edge;
        let mut right = self.right_edge;
        for f in &self.lefts {
            if overlaps(f) {
                left = left.max(f.right);
            }
        }
        for f in &self.rights {
            if overlaps(f) {
                right = right.min(f.left);
            }
        }
        (left, right.max(left))
    }

    /// The smallest float bottom strictly greater than `y` — the next vertical position where the
    /// available band could widen. `None` if no float extends below `y` (caller stops searching).
    fn next_bottom_below(&self, y: f32) -> Option<f32> {
        self.lefts
            .iter()
            .chain(self.rights.iter())
            .map(|f| f.bottom)
            .filter(|&b| b > y + 0.01)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The lowest bottom edge among floats on the cleared side(s), or `min_y` if none — i.e. the y a
    /// `clear`ed box must drop to so it sits below those floats.
    pub(crate) fn clear_to(&self, clear: style::Clear, min_y: f32) -> f32 {
        let mut y = min_y;
        let want_left = matches!(clear, style::Clear::Left | style::Clear::Both);
        let want_right = matches!(clear, style::Clear::Right | style::Clear::Both);
        if want_left {
            for f in &self.lefts {
                y = y.max(f.bottom);
            }
        }
        if want_right {
            for f in &self.rights {
                y = y.max(f.bottom);
            }
        }
        y
    }

    /// Place a float of margin-box size `(width, height)` on `side`, no higher than `min_y`. Scans
    /// downward for the first band wide enough, records the float, and returns its margin-box
    /// top-left `(x, y)`. A float wider than the formatting context is placed flush to its side.
    pub(crate) fn place(
        &mut self,
        width: f32,
        height: f32,
        min_y: f32,
        side: style::Float,
    ) -> (f32, f32) {
        let mut y = min_y;
        let (x, y) = loop {
            let (l, r) = self.band(y, height);
            let avail = r - l;
            if avail >= width || avail >= (self.right_edge - self.left_edge) {
                // Fits in this band (or the band is already the full width — nothing more to gain
                // by dropping further, so place it and let it overflow).
                let x = match side {
                    style::Float::Right => (r - width).max(l),
                    _ => l,
                };
                break (x, y);
            }
            match self.next_bottom_below(y) {
                Some(next) => y = next,
                None => {
                    // No float extends below; place flush to the side at this y.
                    let x = match side {
                        style::Float::Right => (r - width).max(l),
                        _ => l,
                    };
                    break (x, y);
                }
            }
        };
        let rect = FloatRect {
            top: y,
            bottom: y + height.max(0.0),
            left: x,
            right: x + width.max(0.0),
        };
        match side {
            style::Float::Right => self.rights.push(rect),
            _ => self.lefts.push(rect),
        }
        (x, y)
    }

    /// The lowest bottom edge of any placed float (or `fallback` when empty) — used so a container
    /// grows tall enough to contain its floats.
    pub(crate) fn max_bottom(&self, fallback: f32) -> f32 {
        self.lefts
            .iter()
            .chain(self.rights.iter())
            .map(|f| f.bottom)
            .fold(fallback, f32::max)
    }
}
