import AppKit
import CBrowser

// MARK: - BitmapView

/// Displays the engine's RGBA framebuffer. Uses default (non-flipped) coordinates so
/// CGContext.draw renders the top-row-first buffer right-side up. While there's no image
/// yet, paints a near-black background to match the engine's dark scene (no white flash).
final class BitmapView: NSView {
    var image: CGImage?
    /// Called with a vertical delta in points (positive = scroll content toward the end).
    var onScroll: ((CGFloat) -> Void)?
    /// Called with a view-local click point (points, bottom-left origin) on a simple click
    /// (mouse-up that didn't travel far from the mouse-down — i.e. not a drag/selection).
    var onClick: ((CGPoint) -> Void)?
    /// Asked whether a view-local point (points, bottom-left origin) is over a link, so the
    /// cursor can switch to a pointing hand on hover. Returns true if a link is there.
    var isLinkAt: ((CGPoint) -> Bool)?
    /// Called with a key event when the view has focus. Return true if the page consumed it
    /// (e.g. typing into a focused field); false to let it propagate (menu shortcuts, etc.).
    var onKeyDown: ((NSEvent) -> Bool)?
    /// Called with a view-local point as the pointer moves, so the page's hover events can fire.
    var onMove: ((CGPoint) -> Void)?
    /// Called when the pointer leaves the content view (e.g. to hide the hovered-link status bar).
    var onMouseExit: (() -> Void)?
    /// Called with a raw mouse event kind ("mousedown"/"mouseup"/"dblclick"/"contextmenu") + point.
    var onMouseEvent: ((String, CGPoint) -> Void)?
    /// Called on mouse-down with the view-local point to begin a text selection anchor.
    var onSelectStart: ((CGPoint) -> Void)?
    /// Called as the pointer drags to extend the text selection focus.
    var onSelectExtend: ((CGPoint) -> Void)?
    /// Called when a drag ends (the pointer moved beyond the click threshold) to finalize selection.
    var onSelectEnd: ((CGPoint) -> Void)?
    /// Called when the press ended WITHOUT a drag (a plain click) so any selection can be cleared.
    var onSelectCancel: (() -> Void)?

    // Accept keyboard focus so typing into a page text field routes here.
    override var acceptsFirstResponder: Bool { true }

    override func keyDown(with event: NSEvent) {
        if onKeyDown?(event) == true { return }
        super.keyDown(with: event)
    }

    private static let emptyColor = NSColor(calibratedRed: 0.07, green: 0.07, blue: 0.08, alpha: 1.0)

    /// The mouse-down location (view-local), used to distinguish a click from a drag.
    private var mouseDownPoint: CGPoint?
    private var trackingArea: NSTrackingArea?

    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        guard let image = image, let ctx = NSGraphicsContext.current?.cgContext else {
            BitmapView.emptyColor.setFill()
            bounds.fill()
            return
        }
        ctx.draw(image, in: bounds)
    }

    override func scrollWheel(with event: NSEvent) {
        var dy = event.scrollingDeltaY
        // Line-based wheels report small deltas; scale them to roughly a line height.
        if !event.hasPreciseScrollingDeltas { dy *= 16 }
        // Scrolling down (finger/wheel) should reveal content further down the page.
        onScroll?(-dy)
    }

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        mouseDownPoint = p
        onMouseEvent?("mousedown", p)
        // Record the selection anchor here; an actual selection only materializes on drag.
        onSelectStart?(p)
    }

    override func mouseDragged(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        onSelectExtend?(p)
    }

    override func mouseUp(with event: NSEvent) {
        let up = convert(event.locationInWindow, from: nil)
        defer { mouseDownPoint = nil }
        onMouseEvent?("mouseup", up)
        // Treat as a click only if the pointer barely moved (not a drag / text selection).
        if let down = mouseDownPoint {
            let dx = up.x - down.x
            let dy = up.y - down.y
            if (dx * dx + dy * dy) > 16 {
                // A real drag: finalize the text selection and do NOT treat it as a click.
                onSelectEnd?(up)
                return
            }
        }
        // A plain click (no drag): clear any selection so clicking deselects, then handle the click.
        onSelectCancel?()
        onClick?(up)
        if event.clickCount == 2 { onMouseEvent?("dblclick", up) }
    }

    /// Builds the page context menu (Copy / Paste / Inspect / nav). AppKit calls this on a
    /// right-click and pops up the returned menu; we also fire the JS `contextmenu` event.
    var contextMenuProvider: ((CGPoint) -> NSMenu?)?
    override func menu(for event: NSEvent) -> NSMenu? {
        let p = convert(event.locationInWindow, from: nil)
        onMouseEvent?("contextmenu", p)
        return contextMenuProvider?(p)
    }

    // Pointing-hand cursor when hovering a link (nice-to-have).
    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea = trackingArea { removeTrackingArea(trackingArea) }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseMoved, .mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingArea = area
    }

    override func mouseMoved(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        if isLinkAt?(p) == true {
            NSCursor.pointingHand.set()
        } else {
            NSCursor.arrow.set()
        }
        onMove?(p)
    }

    override func mouseExited(with event: NSEvent) {
        NSCursor.arrow.set()
        onMouseExit?()
    }
}

// MARK: - URLTextField

/// A field editor host that lets us keep the focus ring off while still behaving like a
/// normal text field. We disable the focus ring drawing for a clean pill look. We notify
/// the delegate's focus callbacks so the pill can render a subtle active state.
final class URLTextField: NSTextField {
    var onFocusChange: ((Bool) -> Void)?

    override var focusRingType: NSFocusRingType {
        get { .none }
        set { _ = newValue }
    }

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became { onFocusChange?(true) }
        return became
    }

    // The field editor (not the text field) becomes first responder while editing, so we
    // detect end-of-editing via textDidEndEditing instead.
    override func textDidEndEditing(_ notification: Notification) {
        super.textDidEndEditing(notification)
        onFocusChange?(false)
    }
}

// MARK: - HoverButton

/// A borderless button that paints a subtle rounded background on hover for nav/tab affordances.
final class HoverButton: NSButton {
    var hoverBackgroundColor: NSColor = NSColor(white: 0.5, alpha: 0.16)
    private var trackingArea: NSTrackingArea?
    private var hovering = false { didSet { needsDisplay = true } }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea = trackingArea { removeTrackingArea(trackingArea) }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingArea = area
    }

    override func mouseEntered(with event: NSEvent) {
        super.mouseEntered(with: event)
        if isEnabled { hovering = true }
    }

    override func mouseExited(with event: NSEvent) {
        super.mouseExited(with: event)
        hovering = false
    }

    override func draw(_ dirtyRect: NSRect) {
        if hovering && isEnabled {
            let inset = bounds.insetBy(dx: 1, dy: 2)
            let path = NSBezierPath(roundedRect: inset, xRadius: 6, yRadius: 6)
            hoverBackgroundColor.setFill()
            path.fill()
        }
        super.draw(dirtyRect)
    }
}

// MARK: - Content view (observes OS appearance)

/// The window's content view. Overrides `viewDidChangeEffectiveAppearance` (which AppKit fires
/// whenever the effective appearance changes — including the user toggling System Settings →
/// Appearance Light/Dark, or auto day/night) so we can push the new `prefers-color-scheme` into the
/// engine and restyle pages live.
final class ContentView: NSView {
    /// Invoked on every effective-appearance change (Light/Dark toggle). The delegate reads the new
    /// appearance and pushes it to the engines.
    var onAppearanceChange: (() -> Void)?

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        onAppearanceChange?()
    }
}

