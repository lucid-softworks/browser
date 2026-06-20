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

// MARK: - Tab model

/// A single browser tab. Owns its own engine handle and navigation history. The engine is
/// created on init and must be freed exactly once via `freeEngine()` (idempotent).
/// C callback the engine invokes (on the tab's load thread) each time it paints a partial or final
/// frame while a page streams in — this is what makes the page paint progressively before the full
/// download finishes. We copy the pixels synchronously (the pointer is only valid for the duration
/// of the call), then hop to the main thread to display them if the tab is still the active one.
/// A top-level non-capturing function so it bridges to a C function pointer.
private func tabProgressFrame(_ ctx: UnsafeMutableRawPointer?, _ frame: FrameView) {
    guard let ctx = ctx, let pixels = frame.pixels else { return }
    let tab = Unmanaged<Tab>.fromOpaque(ctx).takeUnretainedValue()
    let width = Int(frame.width), height = Int(frame.height), stride = Int(frame.stride)
    let data = Data(bytes: pixels, count: stride * height)
    DispatchQueue.main.async {
        (NSApp.delegate as? AppDelegate)?.displayProgressFrame(
            forTab: tab, data: data, width: width, height: height, stride: stride)
    }
}

final class Tab {
    private(set) var engine: OpaquePointer?
    var urlString: String = ""
    var title: String = "New Tab"

    // Per-tab navigation history.
    var history: [String] = []
    var historyIndex: Int = -1

    var isLoading: Bool = false

    /// Number of loads currently running on a background thread against this engine.
    /// We must not free the engine while any are in flight (would be use-after-free).
    var pendingLoads: Int = 0
    /// Set when the tab is closed but a load is still running; the engine is freed once it drains.
    var freeWhenIdle: Bool = false

    /// Serial queue for ALL engine mutations (loads) on this tab, so two navigations can never
    /// run `browser_engine_load_url` on the same engine concurrently (that would be a data race),
    /// and they apply in order — the latest navigation wins.
    let engineQueue = DispatchQueue(label: "browser.tab.engine")
    /// Bumped on every navigation. A load's completion only applies its result if it's still the
    /// current generation, so a slow earlier load can't clobber a newer navigation.
    var loadGeneration: Int = 0

    // Live runtime stats for the tab tooltip, sampled ~1s by the AppDelegate's stats timer.
    var memBytes: UInt64 = 0
    var cpuPercent: Double = 0
    private var lastCpuNs: UInt64 = 0
    private var lastSampleTS: TimeInterval = 0

    /// Read the engine's heap-used + cumulative-JS-time counters and derive a CPU % over the elapsed
    /// wall-clock since the last sample. Called on the main thread (cheap atomic reads).
    func sampleStats() {
        guard let engine = engine else { return }
        let nowCpu = browser_engine_cpu_ns(engine)
        let nowTS = ProcessInfo.processInfo.systemUptime
        if lastSampleTS > 0 {
            let dCpu = Double(nowCpu &- lastCpuNs)                 // ns of active JS
            let dWall = (nowTS - lastSampleTS) * 1_000_000_000     // ns elapsed
            if dWall > 0 { cpuPercent = max(0, min(100, 100 * dCpu / dWall)) }
        }
        lastCpuNs = nowCpu
        lastSampleTS = nowTS
        memBytes = browser_engine_heap_bytes(engine)
    }

    init() {
        engine = browser_engine_new()
        // Receive progressive frames while pages stream in. ctx is this Tab (unretained: the Tab
        // owns the engine, and we clear the callback in freeEngine before the engine is freed).
        if let engine = engine {
            browser_engine_set_progress_callback(engine, tabProgressFrame, Unmanaged.passUnretained(self).toOpaque())
        }
    }

    /// Free the engine. Safe to call multiple times; subsequent calls are no-ops.
    /// If a background load is in flight, defers the free until that load completes.
    func freeEngine() {
        if pendingLoads > 0 {
            freeWhenIdle = true
            return
        }
        if let engine = engine {
            browser_engine_set_progress_callback(engine, nil, nil) // stop frames before freeing
            browser_engine_free(engine)
        }
        engine = nil
    }

    var canGoBack: Bool { historyIndex > 0 }
    var canGoForward: Bool { historyIndex >= 0 && historyIndex < history.count - 1 }

    func recordHistory(_ url: String) {
        if historyIndex < history.count - 1 {
            history.removeSubrange((historyIndex + 1)...)
        }
        if history.last != url {
            history.append(url)
            historyIndex = history.count - 1
        }
    }
}

// MARK: - TabButton (a single chip in the tab bar)

/// A tab chip: shows a truncated title and a close "×". The close button only shows on the
/// active or hovered chip to keep the strip clean.
final class TabButton: NSView {
    let tab: Tab
    var isActive: Bool = false { didSet { updateAppearance() } }

    var onSelect: (() -> Void)?
    var onClose: (() -> Void)?

    private let titleLabel = NSTextField(labelWithString: "")
    private let closeButton = HoverButton()
    private var trackingArea: NSTrackingArea?
    private var hovering = false { didSet { updateAppearance() } }

    init(tab: Tab) {
        self.tab = tab
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 7
        layer?.cornerCurve = .continuous
        translatesAutoresizingMaskIntoConstraints = false

        titleLabel.font = NSFont.systemFont(ofSize: 12, weight: .medium)
        titleLabel.textColor = NSColor.labelColor
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.maximumNumberOfLines = 1
        titleLabel.cell?.truncatesLastVisibleLine = true
        titleLabel.translatesAutoresizingMaskIntoConstraints = false
        titleLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        addSubview(titleLabel)

        closeButton.isBordered = false
        closeButton.imagePosition = .imageOnly
        closeButton.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: "Close Tab")
        closeButton.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 9, weight: .semibold)
        closeButton.contentTintColor = NSColor.secondaryLabelColor
        closeButton.hoverBackgroundColor = NSColor(white: 0.5, alpha: 0.22)
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        closeButton.target = self
        closeButton.action = #selector(closeClicked)
        closeButton.toolTip = "Close Tab"
        addSubview(closeButton)

        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 28),
            widthAnchor.constraint(greaterThanOrEqualToConstant: 90),
            widthAnchor.constraint(lessThanOrEqualToConstant: 200),

            closeButton.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
            closeButton.centerYAnchor.constraint(equalTo: centerYAnchor),
            closeButton.widthAnchor.constraint(equalToConstant: 18),
            closeButton.heightAnchor.constraint(equalToConstant: 18),

            titleLabel.leadingAnchor.constraint(equalTo: closeButton.trailingAnchor, constant: 4),
            titleLabel.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            titleLabel.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])

        updateTitle()
        updateAppearance()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    func updateTitle() {
        titleLabel.stringValue = tab.title.isEmpty ? "New Tab" : tab.title
        refreshStatsTooltip()
    }

    /// Set the hover tooltip to the URL/title plus the tab's live memory + CPU usage.
    func refreshStatsTooltip() {
        let base = tab.urlString.isEmpty ? (tab.title.isEmpty ? "New Tab" : tab.title) : tab.urlString
        let mb = Double(tab.memBytes) / (1024.0 * 1024.0)
        toolTip = String(format: "%@\n%.1f MB · %.0f%% CPU", base, mb, tab.cpuPercent)
    }

    private func updateAppearance() {
        if isActive {
            layer?.backgroundColor = NSColor(white: 1.0, alpha: 0.16).cgColor
            titleLabel.textColor = NSColor.labelColor
        } else if hovering {
            layer?.backgroundColor = NSColor(white: 1.0, alpha: 0.07).cgColor
            titleLabel.textColor = NSColor.labelColor
        } else {
            layer?.backgroundColor = NSColor.clear.cgColor
            titleLabel.textColor = NSColor.secondaryLabelColor
        }
        // Close button only visible when active or hovered.
        closeButton.isHidden = !(isActive || hovering)
    }

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

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }

    override func mouseDown(with event: NSEvent) {
        // Don't steal clicks that land on the close button.
        let local = convert(event.locationInWindow, from: nil)
        if !closeButton.isHidden && closeButton.frame.insetBy(dx: -2, dy: -2).contains(local) {
            super.mouseDown(with: event)
            return
        }
        onSelect?()
    }

    @objc private func closeClicked() {
        onClose?()
    }
}

// MARK: - DevTools

/// One parsed row of the engine's network log JSON.
private struct NetRow {
    var method: String
    var url: String
    var status: Int
    var ok: Bool
    var ms: Double
    var size: Int
    var type: String

    /// "Name" column: the last non-empty path segment, or the host for "/".
    var name: String {
        guard let u = URL(string: url) else { return url }
        let segs = u.path.split(separator: "/").map(String.init)
        if let last = segs.last, !last.isEmpty { return last }
        return u.host ?? url
    }

    var typeShort: String {
        String(type.split(separator: ";").first ?? Substring(type))
            .trimmingCharacters(in: .whitespaces)
    }

    var statusText: String { status == 0 ? "(failed)" : String(status) }

    var sizeText: String {
        let bytes = Double(size)
        if size <= 0 { return "—" }
        if bytes < 1024 { return "\(size) B" }
        if bytes < 1024 * 1024 { return String(format: "%.1f KB", bytes / 1024) }
        return String(format: "%.1f MB", bytes / (1024 * 1024))
    }

    var timeText: String { "\(Int(ms.rounded())) ms" }
}

/// The bottom devtools panel: a Console tab (page console output + a REPL) and a Network tab
/// (a request table). Hidden by default; toggled with ⌘⌥I. Reaches the active tab's engine and
/// the app's refresh() via injected closures.
/// A thin draggable strip (top edge of the DevTools panel) that reports vertical drag deltas in
/// window points; positive delta = dragged UP = the panel should grow taller.
final class ResizeHandle: NSView {
    var onDrag: ((CGFloat) -> Void)?
    private var lastY: CGFloat?
    override func resetCursorRects() { addCursorRect(bounds, cursor: .resizeUpDown) }
    override func mouseDown(with event: NSEvent) { lastY = event.locationInWindow.y }
    override func mouseDragged(with event: NSEvent) {
        guard let l = lastY else { return }
        let y = event.locationInWindow.y
        onDrag?(y - l) // window coords are bottom-origin: dragging up increases y → grow
        lastY = y
    }
    override func mouseUp(with event: NSEvent) { lastY = nil }
}

final class DevToolsView: NSView {
    /// Returns the active tab's engine, or nil if there is none / a load is in flight.
    var engineProvider: (() -> OpaquePointer?)?
    /// Ask the app to re-render the page (an eval may have mutated the DOM).
    var onRefreshPage: (() -> Void)?
    /// Close the panel (the ✕ button).
    var onCloseDevTools: (() -> Void)?
    /// Drag on the top resize handle: `delta` points (positive = make the panel taller).
    var onResizeDrag: ((CGFloat) -> Void)?

    private let closeButton = NSButton(title: "✕", target: nil, action: nil)

    /// REPL input/output lines, kept Swift-side so they survive console-text refreshes. Cleared
    /// on navigation (a REPL session is per page).
    private var replLines: [String] = []

    private let segmented = NSSegmentedControl(labels: ["Console", "Network", "Elements"], trackingMode: .selectOne, target: nil, action: nil)
    private let header = NSTextField(labelWithString: "")

    // Console tab
    private let consoleScroll = NSScrollView()
    private let consoleText = NSTextView()
    private let promptLabel = NSTextField(labelWithString: "›")
    private let replField = NSTextField()
    private var consoleContainer = NSView()

    // Network tab
    private let netScroll = NSScrollView()
    private let netTable = NSTableView()
    private var netContainer = NSView()
    private var netRows: [NetRow] = []

    // Elements tab
    private let elementsScroll = NSScrollView()
    private let elementsOutline = NSOutlineView()
    private var elementsContainer = NSView()
    /// Root of the parsed DOM tree shown in the outline (nil until first built).
    private var domRoot: DOMNode?
    /// Snapshot of the DOM tree JSON the current `domRoot` was built from, to skip rebuilds when the
    /// page hasn't changed.
    private var domTreeJSON: String = ""

    private static let bg = NSColor(calibratedRed: 0.10, green: 0.10, blue: 0.11, alpha: 1.0)
    private static let mono = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = DevToolsView.bg.cgColor
        buildUI()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override var isFlipped: Bool { true }

    private func buildUI() {
        // Thin top divider.
        let divider = NSBox()
        divider.boxType = .custom
        divider.borderWidth = 0
        divider.fillColor = NSColor.separatorColor
        divider.translatesAutoresizingMaskIntoConstraints = false
        addSubview(divider)

        // Draggable resize handle across the top edge (resize-up-down cursor).
        let resize = ResizeHandle()
        resize.onDrag = { [weak self] delta in self?.onResizeDrag?(delta) }
        resize.translatesAutoresizingMaskIntoConstraints = false
        addSubview(resize)

        // Close (✕) button, top-right.
        closeButton.bezelStyle = .inline
        closeButton.isBordered = false
        closeButton.font = NSFont.systemFont(ofSize: 13)
        closeButton.contentTintColor = NSColor.secondaryLabelColor
        closeButton.target = self
        closeButton.action = #selector(closeDevToolsClicked)
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        addSubview(closeButton)

        // Tab switcher + header count.
        segmented.selectedSegment = 0
        segmented.target = self
        segmented.action = #selector(tabChanged)
        segmented.translatesAutoresizingMaskIntoConstraints = false
        addSubview(segmented)

        header.font = NSFont.systemFont(ofSize: 10)
        header.textColor = NSColor.secondaryLabelColor
        header.translatesAutoresizingMaskIntoConstraints = false
        addSubview(header)

        buildConsoleTab()
        buildNetworkTab()
        buildElementsTab()

        NSLayoutConstraint.activate([
            divider.topAnchor.constraint(equalTo: topAnchor),
            divider.leadingAnchor.constraint(equalTo: leadingAnchor),
            divider.trailingAnchor.constraint(equalTo: trailingAnchor),
            divider.heightAnchor.constraint(equalToConstant: 1),

            resize.topAnchor.constraint(equalTo: topAnchor),
            resize.leadingAnchor.constraint(equalTo: leadingAnchor),
            resize.trailingAnchor.constraint(equalTo: trailingAnchor),
            resize.heightAnchor.constraint(equalToConstant: 6),

            segmented.topAnchor.constraint(equalTo: divider.bottomAnchor, constant: 6),
            segmented.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),

            closeButton.centerYAnchor.constraint(equalTo: segmented.centerYAnchor),
            closeButton.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            closeButton.widthAnchor.constraint(equalToConstant: 22),

            header.centerYAnchor.constraint(equalTo: segmented.centerYAnchor),
            header.trailingAnchor.constraint(equalTo: closeButton.leadingAnchor, constant: -10),

            consoleContainer.topAnchor.constraint(equalTo: segmented.bottomAnchor, constant: 6),
            consoleContainer.leadingAnchor.constraint(equalTo: leadingAnchor),
            consoleContainer.trailingAnchor.constraint(equalTo: trailingAnchor),
            consoleContainer.bottomAnchor.constraint(equalTo: bottomAnchor),

            netContainer.topAnchor.constraint(equalTo: segmented.bottomAnchor, constant: 6),
            netContainer.leadingAnchor.constraint(equalTo: leadingAnchor),
            netContainer.trailingAnchor.constraint(equalTo: trailingAnchor),
            netContainer.bottomAnchor.constraint(equalTo: bottomAnchor),

            elementsContainer.topAnchor.constraint(equalTo: segmented.bottomAnchor, constant: 6),
            elementsContainer.leadingAnchor.constraint(equalTo: leadingAnchor),
            elementsContainer.trailingAnchor.constraint(equalTo: trailingAnchor),
            elementsContainer.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])

        showTab(0)
    }

    private func buildConsoleTab() {
        consoleContainer.translatesAutoresizingMaskIntoConstraints = false
        addSubview(consoleContainer)

        consoleText.isEditable = false
        consoleText.isSelectable = true
        consoleText.drawsBackground = true
        consoleText.backgroundColor = DevToolsView.bg
        consoleText.textColor = NSColor(white: 0.85, alpha: 1.0)
        consoleText.font = DevToolsView.mono
        consoleText.textContainerInset = NSSize(width: 6, height: 4)
        consoleText.isVerticallyResizable = true
        consoleText.isHorizontallyResizable = false
        consoleText.autoresizingMask = [.width]
        consoleText.textContainer?.widthTracksTextView = true

        consoleScroll.documentView = consoleText
        consoleScroll.hasVerticalScroller = true
        consoleScroll.drawsBackground = true
        consoleScroll.backgroundColor = DevToolsView.bg
        consoleScroll.translatesAutoresizingMaskIntoConstraints = false
        consoleContainer.addSubview(consoleScroll)

        promptLabel.font = DevToolsView.mono
        promptLabel.textColor = NSColor(calibratedRed: 0.5, green: 0.8, blue: 1.0, alpha: 1.0)
        promptLabel.translatesAutoresizingMaskIntoConstraints = false
        consoleContainer.addSubview(promptLabel)

        replField.isBezeled = false
        replField.isBordered = false
        replField.drawsBackground = false
        replField.focusRingType = .none
        replField.font = DevToolsView.mono
        replField.textColor = NSColor(white: 0.95, alpha: 1.0)
        replField.placeholderString = "Evaluate JavaScript in the page…"
        replField.usesSingleLineMode = true
        replField.cell?.usesSingleLineMode = true
        replField.target = self
        replField.action = #selector(replSubmit)
        replField.translatesAutoresizingMaskIntoConstraints = false
        consoleContainer.addSubview(replField)

        NSLayoutConstraint.activate([
            consoleScroll.topAnchor.constraint(equalTo: consoleContainer.topAnchor),
            consoleScroll.leadingAnchor.constraint(equalTo: consoleContainer.leadingAnchor),
            consoleScroll.trailingAnchor.constraint(equalTo: consoleContainer.trailingAnchor),

            promptLabel.leadingAnchor.constraint(equalTo: consoleContainer.leadingAnchor, constant: 8),
            promptLabel.bottomAnchor.constraint(equalTo: consoleContainer.bottomAnchor, constant: -6),

            replField.leadingAnchor.constraint(equalTo: promptLabel.trailingAnchor, constant: 6),
            replField.trailingAnchor.constraint(equalTo: consoleContainer.trailingAnchor, constant: -8),
            replField.centerYAnchor.constraint(equalTo: promptLabel.centerYAnchor),

            consoleScroll.bottomAnchor.constraint(equalTo: replField.topAnchor, constant: -6),
        ])
    }

    private func buildNetworkTab() {
        netContainer.translatesAutoresizingMaskIntoConstraints = false
        addSubview(netContainer)

        let cols: [(String, String, CGFloat)] = [
            ("name", "Name", 220),
            ("method", "Method", 70),
            ("status", "Status", 70),
            ("type", "Type", 130),
            ("size", "Size", 80),
            ("time", "Time", 80),
        ]
        for (id, title, width) in cols {
            let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id))
            col.title = title
            col.width = width
            col.minWidth = 40
            netTable.addTableColumn(col)
        }
        netTable.dataSource = self
        netTable.delegate = self
        netTable.usesAlternatingRowBackgroundColors = false
        netTable.backgroundColor = DevToolsView.bg
        netTable.gridColor = NSColor(white: 0.2, alpha: 1.0)
        netTable.gridStyleMask = [.solidHorizontalGridLineMask]
        netTable.rowHeight = 18
        netTable.headerView?.wantsLayer = true

        netScroll.documentView = netTable
        netScroll.hasVerticalScroller = true
        netScroll.drawsBackground = true
        netScroll.backgroundColor = DevToolsView.bg
        netScroll.translatesAutoresizingMaskIntoConstraints = false
        netContainer.addSubview(netScroll)

        NSLayoutConstraint.activate([
            netScroll.topAnchor.constraint(equalTo: netContainer.topAnchor),
            netScroll.leadingAnchor.constraint(equalTo: netContainer.leadingAnchor),
            netScroll.trailingAnchor.constraint(equalTo: netContainer.trailingAnchor),
            netScroll.bottomAnchor.constraint(equalTo: netContainer.bottomAnchor),
        ])
    }

    private func buildElementsTab() {
        elementsContainer.translatesAutoresizingMaskIntoConstraints = false
        addSubview(elementsContainer)

        let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("node"))
        col.title = "DOM"
        col.resizingMask = [.autoresizingMask]
        elementsOutline.addTableColumn(col)
        elementsOutline.outlineTableColumn = col
        elementsOutline.headerView = nil
        elementsOutline.dataSource = self
        elementsOutline.delegate = self
        elementsOutline.backgroundColor = DevToolsView.bg
        elementsOutline.usesAlternatingRowBackgroundColors = false
        elementsOutline.rowHeight = 18
        elementsOutline.indentationPerLevel = 14
        elementsOutline.autoresizesOutlineColumn = false
        elementsOutline.action = #selector(elementsRowClicked)
        elementsOutline.target = self

        elementsScroll.documentView = elementsOutline
        elementsScroll.hasVerticalScroller = true
        elementsScroll.hasHorizontalScroller = true
        elementsScroll.drawsBackground = true
        elementsScroll.backgroundColor = DevToolsView.bg
        elementsScroll.translatesAutoresizingMaskIntoConstraints = false
        elementsContainer.addSubview(elementsScroll)

        NSLayoutConstraint.activate([
            elementsScroll.topAnchor.constraint(equalTo: elementsContainer.topAnchor),
            elementsScroll.leadingAnchor.constraint(equalTo: elementsContainer.leadingAnchor),
            elementsScroll.trailingAnchor.constraint(equalTo: elementsContainer.trailingAnchor),
            elementsScroll.bottomAnchor.constraint(equalTo: elementsContainer.bottomAnchor),
        ])
    }

    // MARK: Tab switching

    @objc private func tabChanged() { showTab(segmented.selectedSegment) }
    @objc private func closeDevToolsClicked() { onCloseDevTools?() }

    private func showTab(_ index: Int) {
        consoleContainer.isHidden = index != 0
        netContainer.isHidden = index != 1
        elementsContainer.isHidden = index != 2
        // Leaving the Elements tab clears the on-page highlight.
        if index != 2 {
            if let engine = engineProvider?() {
                browser_engine_set_inspect_node(engine, -1)
            }
            onRefreshPage?()
        }
        refreshVisible()
    }

    var isConsoleTab: Bool { segmented.selectedSegment == 0 }
    var isElementsTab: Bool { segmented.selectedSegment == 2 }

    // MARK: REPL

    @objc private func replSubmit() {
        let input = replField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !input.isEmpty else { return }
        replLines.append("› " + input)
        if let engine = engineProvider?() {
            let result = input.withCString { browser_engine_console_eval(engine, $0) }
            if let result = result {
                replLines.append(String(cString: result))
            }
        }
        replField.stringValue = ""
        // An eval may have changed the DOM; ask the app to re-render (which also refreshes us).
        onRefreshPage?()
        refreshConsole()
        scrollConsoleToBottom()
        // Keep focus in the REPL for the next expression.
        window?.makeFirstResponder(replField)
    }

    /// Move focus into the REPL field (called when devtools is shown on the Console tab).
    func focusREPL() {
        if isConsoleTab { window?.makeFirstResponder(replField) }
    }

    // MARK: Refresh

    /// Clear the per-page REPL session (called on navigation / active-tab change).
    func clearREPL() {
        replLines.removeAll()
        refreshConsole()
    }

    /// Refresh whichever tab is currently visible. Cheap; called on the render/tick path.
    func refreshVisible() {
        guard !isHidden else { return }
        if isConsoleTab {
            refreshConsole()
        } else if isElementsTab {
            refreshElements()
        } else {
            refreshNetwork()
        }
    }

    /// Rebuild the DOM outline from the engine when the page changed, then refresh the row count
    /// header. Only runs while the Elements tab is visible (cheap-guard); rebuilds only when the DOM
    /// tree JSON differs from what's currently shown, so it's not re-parsed every tick.
    private func refreshElements(force: Bool = false) {
        guard !isHidden, isElementsTab else { return }
        var json = "{}"
        if let engine = engineProvider?(), let c = browser_engine_dom_tree(engine) {
            json = String(cString: c)
        }
        if force || json != domTreeJSON || domRoot == nil {
            domTreeJSON = json
            domRoot = DOMNode.parse(json)
            elementsOutline.reloadData()
            // Expand the top couple of levels so the tree isn't a single collapsed root.
            if let root = domRoot {
                elementsOutline.expandItem(root)
                for child in root.children { elementsOutline.expandItem(child) }
            }
        }
        let count = domRoot?.descendantCount ?? 0
        header.stringValue = "\(count) node\(count == 1 ? "" : "s")"
    }

    private func refreshConsole() {
        guard !isHidden, isConsoleTab else { return }
        var lines: [String] = []
        if let engine = engineProvider?(), let c = browser_engine_console_text(engine) {
            let text = String(cString: c)
            if !text.isEmpty { lines = text.components(separatedBy: "\n") }
        }
        lines.append(contentsOf: replLines)

        let attr = NSMutableAttributedString()
        let normal = NSColor(white: 0.85, alpha: 1.0)
        let errColor = NSColor(calibratedRed: 1.0, green: 0.45, blue: 0.45, alpha: 1.0)
        let replColor = NSColor(calibratedRed: 0.55, green: 0.85, blue: 1.0, alpha: 1.0)
        for (i, line) in lines.enumerated() {
            let lower = line.lowercased()
            let color: NSColor
            if line.hasPrefix("›") {
                color = replColor
            } else if lower.contains("uncaught") || lower.contains("error") {
                color = errColor
            } else {
                color = normal
            }
            let suffix = i == lines.count - 1 ? "" : "\n"
            attr.append(NSAttributedString(string: line + suffix, attributes: [
                .font: DevToolsView.mono,
                .foregroundColor: color,
            ]))
        }
        consoleText.textStorage?.setAttributedString(attr)
    }

    private func scrollConsoleToBottom() {
        consoleText.scrollToEndOfDocument(nil)
    }

    private func refreshNetwork() {
        guard !isHidden, !isConsoleTab else { return }
        var rows: [NetRow] = []
        if let engine = engineProvider?(), let c = browser_engine_network_log(engine) {
            let json = String(cString: c)
            if let data = json.data(using: .utf8),
               let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] {
                for o in arr {
                    rows.append(NetRow(
                        method: o["method"] as? String ?? "",
                        url: o["url"] as? String ?? "",
                        status: (o["status"] as? NSNumber)?.intValue ?? 0,
                        ok: o["ok"] as? Bool ?? false,
                        ms: (o["ms"] as? NSNumber)?.doubleValue ?? 0,
                        size: (o["size"] as? NSNumber)?.intValue ?? 0,
                        type: o["type"] as? String ?? ""
                    ))
                }
            }
        }
        netRows = rows
        header.stringValue = "\(rows.count) request\(rows.count == 1 ? "" : "s")"
        netTable.reloadData()
    }

    // MARK: Elements selection / inspect

    /// Selection in the outline → highlight the node on the page. Clicking empty space clears it.
    @objc private func elementsRowClicked() {
        let row = elementsOutline.selectedRow
        guard let engine = engineProvider?() else { return }
        if row >= 0, let node = elementsOutline.item(atRow: row) as? DOMNode {
            browser_engine_set_inspect_node(engine, Int64(node.id))
        } else {
            browser_engine_set_inspect_node(engine, -1)
        }
        onRefreshPage?()
    }

    /// "Inspect Element" entry point: switch to the Elements tab, (re)build the tree if needed, then
    /// find, expand-to, select, scroll-to, and highlight the row for `nodeId`.
    func inspect(nodeId: Int) {
        segmented.selectedSegment = 2
        showTab(2)
        refreshElements(force: true)
        guard let root = domRoot, let target = root.find(id: nodeId) else { return }
        // Expand every ancestor so the target row is visible, then select + scroll to it.
        for ancestor in root.ancestors(of: target) {
            elementsOutline.expandItem(ancestor)
        }
        let row = elementsOutline.row(forItem: target)
        guard row >= 0 else { return }
        elementsOutline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        elementsOutline.scrollRowToVisible(row)
        if let engine = engineProvider?() {
            browser_engine_set_inspect_node(engine, Int64(target.id))
        }
        onRefreshPage?()
    }
}

extension DevToolsView: NSTableViewDataSource, NSTableViewDelegate {
    func numberOfRows(in tableView: NSTableView) -> Int { netRows.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let column = tableColumn, row < netRows.count else { return nil }
        let r = netRows[row]
        let id = column.identifier.rawValue
        let cell: NSTextField
        if let reused = tableView.makeView(withIdentifier: column.identifier, owner: self) as? NSTextField {
            cell = reused
        } else {
            cell = NSTextField(labelWithString: "")
            cell.identifier = column.identifier
            cell.font = DevToolsView.mono
            cell.lineBreakMode = .byTruncatingTail
            cell.drawsBackground = false
        }
        var color = NSColor(white: 0.85, alpha: 1.0)
        switch id {
        case "name": cell.stringValue = r.name
        case "method": cell.stringValue = r.method
        case "status":
            cell.stringValue = r.statusText
            if r.status == 0 || !(r.status >= 200 && r.status < 300) {
                color = NSColor(calibratedRed: 1.0, green: 0.45, blue: 0.45, alpha: 1.0)
            }
        case "type": cell.stringValue = r.typeShort
        case "size": cell.stringValue = r.sizeText
        case "time": cell.stringValue = r.timeText
        default: cell.stringValue = ""
        }
        cell.textColor = color
        cell.toolTip = r.url
        return cell
    }
}

// MARK: - DOM tree model (Elements tab)

/// A lightweight, reference-typed node parsed from the engine's `browser_engine_dom_tree` JSON.
/// Reference type so `NSOutlineView` can use the instances as opaque items (identity-based).
final class DOMNode {
    let id: Int
    let isElement: Bool
    let tag: String
    let attrs: [(String, String)]
    let text: String
    let children: [DOMNode]
    weak var parent: DOMNode?

    /// True for the synthetic `</tag>` row shown after an expanded element's children.
    var isClosing = false
    private var _closingNode: DOMNode?
    /// A cached `</tag>` pseudo-node (same id, so selecting it highlights the same element).
    var closingNode: DOMNode {
        if let n = _closingNode { return n }
        let n = DOMNode(id: id, isElement: true, tag: tag, attrs: [], text: "", children: [])
        n.isClosing = true
        n.parent = self
        _closingNode = n
        return n
    }

    init(id: Int, isElement: Bool, tag: String, attrs: [(String, String)], text: String, children: [DOMNode]) {
        self.id = id
        self.isElement = isElement
        self.tag = tag
        self.attrs = attrs
        self.text = text
        self.children = children
        for c in children { c.parent = self }
    }

    /// Parse the engine's DOM tree JSON (a single nested object) into a `DOMNode`, or nil on
    /// `"{}"` / malformed input.
    static func parse(_ json: String) -> DOMNode? {
        guard let data = json.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return nil
        }
        return build(obj)
    }

    private static func build(_ o: [String: Any]) -> DOMNode? {
        let id = (o["id"] as? NSNumber)?.intValue ?? -1
        let type = o["type"] as? String ?? ""
        if type == "text" {
            return DOMNode(id: id, isElement: false, tag: "", attrs: [],
                           text: o["text"] as? String ?? "", children: [])
        }
        guard type == "element" else { return nil }
        var attrs: [(String, String)] = []
        if let a = o["attrs"] as? [String: Any] {
            // Preferred order: id, class, then the rest alphabetically.
            let ordered = a.keys.sorted { lhs, rhs in
                func rank(_ k: String) -> Int { k == "id" ? 0 : (k == "class" ? 1 : 2) }
                let (rl, rr) = (rank(lhs), rank(rhs))
                return rl != rr ? rl < rr : lhs < rhs
            }
            for k in ordered { attrs.append((k, (a[k] as? String) ?? "")) }
        }
        var kids: [DOMNode] = []
        if let cs = o["children"] as? [[String: Any]] {
            for c in cs { if let n = build(c) { kids.append(n) } }
        }
        return DOMNode(id: id, isElement: true, tag: o["tag"] as? String ?? "", attrs: attrs,
                       text: "", children: kids)
    }

    /// Total node count of this subtree (including self), for the header label.
    var descendantCount: Int {
        1 + children.reduce(0) { $0 + $1.descendantCount }
    }

    /// Depth-first search for a node by its engine NodeId.
    func find(id target: Int) -> DOMNode? {
        if id == target { return self }
        for c in children { if let f = c.find(id: target) { return f } }
        return nil
    }

    /// The chain of ancestors of `node` (root-first, excluding `node` itself).
    func ancestors(of node: DOMNode) -> [DOMNode] {
        var chain: [DOMNode] = []
        var cur = node.parent
        while let c = cur { chain.append(c); cur = c.parent }
        return chain.reversed()
    }
}

extension DevToolsView: NSOutlineViewDataSource, NSOutlineViewDelegate {
    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        guard let node = item as? DOMNode else { return domRoot == nil ? 0 : 1 }
        if node.isClosing || node.children.isEmpty { return 0 }
        return node.children.count + 1 // +1 for the synthetic </tag> row
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        guard let node = item as? DOMNode else { return domRoot! }
        return index < node.children.count ? node.children[index] : node.closingNode
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        guard let node = item as? DOMNode, !node.isClosing else { return false }
        return !node.children.isEmpty
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let node = item as? DOMNode else { return nil }
        let id = NSUserInterfaceItemIdentifier("domCell")
        let cell: NSTextField
        if let reused = outlineView.makeView(withIdentifier: id, owner: self) as? NSTextField {
            cell = reused
        } else {
            cell = NSTextField(labelWithString: "")
            cell.identifier = id
            cell.font = DevToolsView.mono
            cell.lineBreakMode = .byTruncatingTail
            cell.drawsBackground = false
        }
        if node.isClosing {
            cell.attributedStringValue = DevToolsView.closingRow(node)
        } else if node.isElement {
            cell.attributedStringValue = DevToolsView.elementRow(node)
        } else {
            // Text node: quoted, truncated, muted.
            let t = DevToolsView.truncate(node.text, 80)
            cell.stringValue = "\"\(t)\""
            cell.textColor = NSColor(white: 0.55, alpha: 1.0)
        }
        return cell
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        // Selection can change via keyboard too; mirror the click handler.
        let row = elementsOutline.selectedRow
        guard let engine = engineProvider?() else { return }
        if row >= 0, let node = elementsOutline.item(atRow: row) as? DOMNode {
            browser_engine_set_inspect_node(engine, Int64(node.id))
        } else {
            browser_engine_set_inspect_node(engine, -1)
        }
        onRefreshPage?()
    }

    // MARK: Row rendering

    /// An HTML-ish opening tag for an element row, e.g. `<div id="main" class="box">`, with the tag
    /// name and attribute names/values colored.
    static func elementRow(_ node: DOMNode) -> NSAttributedString {
        let tagColor = NSColor(calibratedRed: 0.45, green: 0.75, blue: 1.0, alpha: 1.0)
        let attrNameColor = NSColor(calibratedRed: 0.85, green: 0.65, blue: 0.45, alpha: 1.0)
        let attrValColor = NSColor(calibratedRed: 0.55, green: 0.80, blue: 0.55, alpha: 1.0)
        let punct = NSColor(white: 0.55, alpha: 1.0)
        let font = DevToolsView.mono

        let s = NSMutableAttributedString()
        func add(_ str: String, _ color: NSColor) {
            s.append(NSAttributedString(string: str, attributes: [.font: font, .foregroundColor: color]))
        }
        add("<", punct)
        add(node.tag, tagColor)
        // Show up to a few attributes (already ordered id, class, then the rest); truncate values.
        for (k, v) in node.attrs.prefix(6) {
            add(" ", punct)
            add(k, attrNameColor)
            add("=\"", punct)
            add(truncate(v, 40), attrValColor)
            add("\"", punct)
        }
        if node.attrs.count > 6 { add(" …", punct) }
        add(">", punct)
        return s
    }

    /// A `</tag>` closing-tag row (shown after an expanded element's children).
    static func closingRow(_ node: DOMNode) -> NSAttributedString {
        let tagColor = NSColor(calibratedRed: 0.45, green: 0.75, blue: 1.0, alpha: 1.0)
        let punct = NSColor(white: 0.55, alpha: 1.0)
        let font = DevToolsView.mono
        let s = NSMutableAttributedString()
        func add(_ str: String, _ color: NSColor) {
            s.append(NSAttributedString(string: str, attributes: [.font: font, .foregroundColor: color]))
        }
        add("</", punct); add(node.tag, tagColor); add(">", punct)
        return s
    }

    static func truncate(_ s: String, _ max: Int) -> String {
        let collapsed = s.replacingOccurrences(of: "\n", with: " ")
        if collapsed.count <= max { return collapsed }
        return String(collapsed.prefix(max)) + "…"
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

// MARK: - AppDelegate

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    var urlField: URLTextField!
    var bitmapView: BitmapView!

    // DevTools bottom panel (hidden by default; ⌘⌥I toggles).
    private var devTools: DevToolsView!
    private var devToolsVisible = false
    private var devToolsHeightConstraint: NSLayoutConstraint!
    private var bitmapBottomToContent: NSLayoutConstraint!
    private var bitmapBottomToDevTools: NSLayoutConstraint!
    private let devToolsHeight: CGFloat = 260

    private var backButton: NSButton!
    private var forwardButton: NSButton!
    private var reloadButton: NSButton!
    private var progress: NSProgressIndicator!
    private var pill: NSView!
    private var lockSymbol: NSImageView!

    // Tab bar
    private var tabStack: NSStackView!
    private var newTabButton: HoverButton!
    private var tabButtons: [TabButton] = []

    // MARK: Tab state
    private var tabs: [Tab] = []
    private var activeIndex: Int = 0

    private var activeTab: Tab? {
        guard activeIndex >= 0, activeIndex < tabs.count else { return nil }
        return tabs[activeIndex]
    }

    private var inFlightLoads = 0
    /// Coalesces rapid live-resize events so we re-layout once the drag settles.
    private var resizeWork: DispatchWorkItem?

    private let defaultURL = "https://browserscore.dev"
    private let toolbarHeight: CGFloat = 48
    private let tabBarHeight: CGFloat = 36

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()

        let contentRect = NSRect(x: 0, y: 0, width: 1100, height: 780)
        window = NSWindow(
            contentRect: contentRect,
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.title = "browser"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovableByWindowBackground = false
        window.minSize = NSSize(width: 380, height: 300)
        window.center() // first-run default; a saved frame is restored last (just before show).

        let content = ContentView(frame: contentRect)
        content.wantsLayer = true
        content.onAppearanceChange = { [weak self] in self?.applyColorScheme() }
        window.contentView = content

        // MARK: Toolbar (translucent visual effect background)
        let toolbar = NSVisualEffectView()
        toolbar.material = .titlebar
        toolbar.blendingMode = .withinWindow
        toolbar.state = .followsWindowActiveState
        toolbar.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(toolbar)

        // MARK: Tab bar (slim translucent strip under the toolbar)
        let tabBar = NSVisualEffectView()
        tabBar.material = .headerView
        tabBar.blendingMode = .withinWindow
        tabBar.state = .followsWindowActiveState
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(tabBar)

        tabStack = NSStackView()
        tabStack.orientation = .horizontal
        tabStack.spacing = 4
        tabStack.alignment = .centerY
        tabStack.distribution = .fill
        tabStack.translatesAutoresizingMaskIntoConstraints = false
        tabBar.addSubview(tabStack)

        newTabButton = HoverButton()
        newTabButton.isBordered = false
        newTabButton.imagePosition = .imageOnly
        newTabButton.image = NSImage(systemSymbolName: "plus", accessibilityDescription: "New Tab")
        newTabButton.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 12, weight: .medium)
        newTabButton.contentTintColor = NSColor.labelColor
        newTabButton.target = self
        newTabButton.action = #selector(newTab)
        newTabButton.toolTip = "New Tab"
        newTabButton.translatesAutoresizingMaskIntoConstraints = false
        tabBar.addSubview(newTabButton)

        // A subtle hairline separator under the tab bar.
        let separator = NSBox()
        separator.boxType = .custom
        separator.borderWidth = 0
        separator.fillColor = NSColor.separatorColor
        separator.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(separator)

        // MARK: Navigation buttons
        backButton = makeNavButton(symbol: "chevron.backward", description: "Back", action: #selector(goBack))
        forwardButton = makeNavButton(symbol: "chevron.forward", description: "Forward", action: #selector(goForward))
        reloadButton = makeNavButton(symbol: "arrow.clockwise", description: "Reload", action: #selector(reload))

        let navStack = NSStackView(views: [backButton, forwardButton, reloadButton])
        navStack.orientation = .horizontal
        navStack.spacing = 2
        navStack.alignment = .centerY
        navStack.translatesAutoresizingMaskIntoConstraints = false
        toolbar.addSubview(navStack)

        // MARK: Address bar (pill)
        urlField = URLTextField()
        urlField.stringValue = ""
        urlField.placeholderString = "Search or enter address"
        urlField.isBezeled = false
        urlField.isBordered = false
        urlField.drawsBackground = false
        urlField.focusRingType = .none
        urlField.font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        urlField.textColor = NSColor.labelColor
        urlField.alignment = .left
        urlField.lineBreakMode = .byTruncatingTail
        // Let the field clip (it truncates) instead of resisting compression — otherwise its text
        // width holds the pill near its max and pins a min window width (you couldn't shrink it).
        urlField.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        urlField.setContentHuggingPriority(.defaultLow, for: .horizontal)
        urlField.usesSingleLineMode = true
        urlField.cell?.usesSingleLineMode = true
        urlField.cell?.wraps = false
        urlField.cell?.isScrollable = true
        urlField.target = self
        urlField.action = #selector(navigate(_:))
        urlField.translatesAutoresizingMaskIntoConstraints = false
        urlField.onFocusChange = { [weak self] focused in
            self?.setAddressBarFocused(focused)
        }

        // Decorative leading lock/globe symbol inside the pill.
        lockSymbol = NSImageView()
        lockSymbol.image = NSImage(systemSymbolName: "globe", accessibilityDescription: nil)
        lockSymbol.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 11, weight: .regular)
        lockSymbol.contentTintColor = NSColor.secondaryLabelColor
        lockSymbol.translatesAutoresizingMaskIntoConstraints = false

        // Container gives the pill its rounded background + padding.
        pill = NSView()
        pill.wantsLayer = true
        pill.layer?.cornerRadius = 9
        pill.layer?.cornerCurve = .continuous
        pill.layer?.borderWidth = 1
        pill.translatesAutoresizingMaskIntoConstraints = false
        pill.addSubview(lockSymbol)
        pill.addSubview(urlField)
        toolbar.addSubview(pill)
        setAddressBarFocused(false)

        // MARK: Loading indicator
        progress = NSProgressIndicator()
        progress.style = .spinning
        progress.controlSize = .small
        progress.isDisplayedWhenStopped = false
        progress.isIndeterminate = true
        progress.translatesAutoresizingMaskIntoConstraints = false
        toolbar.addSubview(progress)

        // MARK: Bitmap content view
        bitmapView = BitmapView()
        bitmapView.translatesAutoresizingMaskIntoConstraints = false
        bitmapView.onScroll = { [weak self] dyPoints in self?.scrollActiveTab(dyPoints) }
        bitmapView.onClick = { [weak self] point in self?.handleContentClick(point) }
        bitmapView.isLinkAt = { [weak self] point in self?.linkURL(at: point) != nil }
        bitmapView.onKeyDown = { [weak self] event in self?.handleKeyDown(event) ?? false }
        bitmapView.onMove = { [weak self] point in self?.handleContentMove(point) }
        bitmapView.onMouseEvent = { [weak self] kind, point in self?.handleMouseEvent(kind, point) }
        bitmapView.onSelectStart = { [weak self] point in self?.handleSelectStart(point) }
        bitmapView.onSelectExtend = { [weak self] point in self?.handleSelectExtend(point) }
        bitmapView.onSelectEnd = { [weak self] point in self?.handleSelectExtend(point) }
        bitmapView.onSelectCancel = { [weak self] in self?.handleSelectCancel() }
        bitmapView.contextMenuProvider = { [weak self] point in self?.buildContextMenu(at: point) }
        content.addSubview(bitmapView)

        // MARK: DevTools panel (hidden by default; ⌘⌥I toggles)
        devTools = DevToolsView()
        devTools.translatesAutoresizingMaskIntoConstraints = false
        devTools.isHidden = true
        devTools.engineProvider = { [weak self] in
            guard let tab = self?.activeTab, let engine = tab.engine, tab.pendingLoads == 0 else { return nil }
            return engine
        }
        devTools.onRefreshPage = { [weak self] in self?.refresh() }
        devTools.onCloseDevTools = { [weak self] in if self?.devToolsVisible == true { self?.toggleDevTools() } }
        devTools.onResizeDrag = { [weak self] delta in self?.resizeDevTools(by: delta) }
        content.addSubview(devTools)

        // MARK: Auto Layout
        NSLayoutConstraint.activate([
            toolbar.topAnchor.constraint(equalTo: content.topAnchor),
            toolbar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            toolbar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            toolbar.heightAnchor.constraint(equalToConstant: toolbarHeight),

            tabBar.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
            tabBar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            tabBar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            tabBar.heightAnchor.constraint(equalToConstant: tabBarHeight),

            tabStack.leadingAnchor.constraint(equalTo: tabBar.leadingAnchor, constant: 8),
            tabStack.centerYAnchor.constraint(equalTo: tabBar.centerYAnchor),
            tabStack.trailingAnchor.constraint(lessThanOrEqualTo: newTabButton.leadingAnchor, constant: -6),

            newTabButton.leadingAnchor.constraint(equalTo: tabStack.trailingAnchor, constant: 6),
            newTabButton.centerYAnchor.constraint(equalTo: tabBar.centerYAnchor),
            newTabButton.widthAnchor.constraint(equalToConstant: 26),
            newTabButton.heightAnchor.constraint(equalToConstant: 24),

            separator.topAnchor.constraint(equalTo: tabBar.bottomAnchor),
            separator.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            separator.heightAnchor.constraint(equalToConstant: 1),

            bitmapView.topAnchor.constraint(equalTo: separator.bottomAnchor),
            bitmapView.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            bitmapView.trailingAnchor.constraint(equalTo: content.trailingAnchor),

            // Nav buttons pinned to the leading edge, clear of the traffic lights.
            navStack.leadingAnchor.constraint(equalTo: toolbar.leadingAnchor, constant: 80),
            navStack.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),

            // Spinner trails the address bar on the right.
            progress.trailingAnchor.constraint(equalTo: toolbar.trailingAnchor, constant: -16),
            progress.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            progress.widthAnchor.constraint(equalToConstant: 16),
            progress.heightAnchor.constraint(equalToConstant: 16),

            // Pill: vertically centered; horizontal centering is applied below at a low priority
            // (so it yields to the required nav/spinner gaps and the window can still shrink).
            pill.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            pill.heightAnchor.constraint(equalToConstant: 32),

            lockSymbol.leadingAnchor.constraint(equalTo: pill.leadingAnchor, constant: 12),
            lockSymbol.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
            lockSymbol.widthAnchor.constraint(equalToConstant: 14),

            urlField.leadingAnchor.constraint(equalTo: lockSymbol.trailingAnchor, constant: 8),
            urlField.trailingAnchor.constraint(equalTo: pill.trailingAnchor, constant: -14),
            urlField.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
        ])

        // Pill width: prefers a fixed comfortable width (capped), compressing on narrow windows.
        // NOTE: do NOT tie this to toolbar.width (e.g. width == toolbar.width * k) — paired with the
        // required max it pins the window's resizable WIDTH to a narrow band (you could only resize
        // vertically). A breakable constant lets the window grow freely (pill caps, extra space sits
        // on the sides, like Safari) and shrink until the required nav/spinner gaps stop it.
        // The pill must NOT prefer any absolute width: a `pill.width == K` constraint (at ANY
        // priority > the fitting threshold ~50) contributes K to the window's enforced minimum
        // width, so the window can't shrink below it. So the pill is CENTERED and gets its width
        // ONLY from RELATIVE constraints (symmetric toolbar-edge fills, capped at 640) — no absolute
        // width preference, hence no fixed min-width floor.
        let pillMaxWidth = pill.widthAnchor.constraint(lessThanOrEqualToConstant: 640)
        pillMaxWidth.priority = .required
        // Never overlap nav / spinner (required clearances). On a narrow window these push the
        // centered pill smaller; the window can still shrink to ~where a 0-width centered pill clears
        // the nav buttons (just above minSize).
        let pillLeadingGap = pill.leadingAnchor.constraint(greaterThanOrEqualTo: navStack.trailingAnchor, constant: 16)
        let pillTrailingGap = pill.trailingAnchor.constraint(lessThanOrEqualTo: progress.leadingAnchor, constant: -16)
        // Centered (required) — symmetric, so it doesn't favor the nav side and doesn't pin a wide
        // minimum the way an absolute width would.
        let pillCenterX = pill.centerXAnchor.constraint(equalTo: toolbar.centerXAnchor)
        // Width source: symmetric low-priority pulls toward BOTH toolbar edges (relative → no floor).
        // They stretch the pill to the 640 cap when there's room and yield to the required clearances
        // when narrow, keeping it centered throughout.
        let pillFillLeading = pill.leadingAnchor.constraint(equalTo: toolbar.leadingAnchor, constant: 16)
        pillFillLeading.priority = .defaultLow
        let pillFillTrailing = pill.trailingAnchor.constraint(equalTo: toolbar.trailingAnchor, constant: -16)
        pillFillTrailing.priority = .defaultLow
        NSLayoutConstraint.activate([pillMaxWidth, pillLeadingGap, pillTrailingGap, pillCenterX, pillFillLeading, pillFillTrailing])

        // DevTools sits below the bitmap, splitting the content area vertically. We toggle which
        // of two bitmap-bottom constraints is active: when hidden the bitmap fills to the content
        // bottom; when shown it stops at the devtools top and devtools takes a fixed height.
        bitmapBottomToContent = bitmapView.bottomAnchor.constraint(equalTo: content.bottomAnchor)
        bitmapBottomToDevTools = bitmapView.bottomAnchor.constraint(equalTo: devTools.topAnchor)
        devToolsHeightConstraint = devTools.heightAnchor.constraint(equalToConstant: devToolsHeight)
        NSLayoutConstraint.activate([
            devTools.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            devTools.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            devTools.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            devToolsHeightConstraint,
            bitmapBottomToContent,
        ])

        // Only listen for resize/backing callbacks once all views exist, so an early
        // notification can't reach updateViewport() before bitmapView is set.
        window.delegate = self

        // Restore the saved position/size/monitor LAST — after the whole view tree + constraints
        // exist — so a layout pass (or AppKit re-constraining a secondary-screen frame) can't shift
        // the window right after it's placed (which looked like "it moves when you interact"). The
        // saved frame is in global coords so it encodes the display; AppKit clamps it back onto an
        // available screen if that monitor is gone. setFrameAutosaveName also persists it on every
        // move/resize (current at close); no-op restore on first run keeps the centered default.
        window.setFrameAutosaveName("BrowserMainWindow")
        window.setFrameUsingName("BrowserMainWindow")
        window.makeKeyAndOrderFront(nil)

        // Create the first tab (becomes active) and start loading the default URL.
        createTab(initialURL: defaultURL, focusAddressBar: false)
        updateViewport()
        applyColorScheme() // push the real OS appearance to the engine before the first load
        refresh()
        if let url = activeTab?.urlString, !url.isEmpty {
            load(urlString: url, recordHistory: true)
        }

        // Pump the active page's JS event loop (~20fps): runs due setTimeout/setInterval/rAF
        // callbacks in the live runtime. A cheap no-op when nothing is due; repaints only when
        // the DOM actually changed. Skipped while a load is running (engine busy on its queue).
        tickTimer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { [weak self] _ in
            guard let self = self, let tab = self.activeTab, let engine = tab.engine,
                  tab.pendingLoads == 0 else { return }
            if browser_engine_tick(engine) != 0 { self.refresh() }
        }

        // Sample each tab's memory + CPU once a second and refresh the tab tooltips, so hovering a
        // tab shows its current usage.
        statsTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            for tab in self.tabs { tab.sampleStats() }
            for button in self.tabButtons { button.refreshStatsTooltip() }
        }
    }

    /// Repeating timer that pumps the active page's JS event loop. Retained for the app's lifetime.
    private var tickTimer: Timer?
    /// Repeating timer that samples per-tab CPU/memory for the tab tooltips.
    private var statsTimer: Timer?

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
        // Free every tab's engine exactly once. freeEngine() is idempotent.
        for tab in tabs {
            tab.freeEngine()
        }
        tabs.removeAll()
    }

    // MARK: UI helpers

    private func makeNavButton(symbol: String, description: String, action: Selector) -> NSButton {
        let button = HoverButton()
        button.translatesAutoresizingMaskIntoConstraints = false
        button.isBordered = false
        button.imagePosition = .imageOnly
        button.image = NSImage(systemSymbolName: symbol, accessibilityDescription: description)
        button.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 14, weight: .medium)
        button.contentTintColor = NSColor.labelColor
        button.target = self
        button.action = action
        button.toolTip = description
        NSLayoutConstraint.activate([
            button.widthAnchor.constraint(equalToConstant: 30),
            button.heightAnchor.constraint(equalToConstant: 28),
        ])
        return button
    }

    /// Toggle the address bar's focused/active visual state.
    private func setAddressBarFocused(_ focused: Bool) {
        guard let pill = pill else { return }
        if focused {
            pill.layer?.backgroundColor = NSColor(white: 0.5, alpha: 0.20).cgColor
            pill.layer?.borderColor = NSColor.controlAccentColor.withAlphaComponent(0.85).cgColor
        } else {
            pill.layer?.backgroundColor = NSColor(white: 0.5, alpha: 0.14).cgColor
            pill.layer?.borderColor = NSColor.clear.cgColor
        }
    }

    private func buildMenu() {
        let mainMenu = NSMenu()

        // App menu
        let appMenuItem = NSMenuItem()
        mainMenu.addItem(appMenuItem)
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit browser", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appMenuItem.submenu = appMenu

        // File menu (tab management).
        let fileMenuItem = NSMenuItem()
        mainMenu.addItem(fileMenuItem)
        let fileMenu = NSMenu(title: "File")
        let newTabItem = NSMenuItem(title: "New Tab", action: #selector(newTab), keyEquivalent: "t")
        newTabItem.target = self
        fileMenu.addItem(newTabItem)
        let closeTabItem = NSMenuItem(title: "Close Tab", action: #selector(closeCurrentTab), keyEquivalent: "w")
        closeTabItem.target = self
        fileMenu.addItem(closeTabItem)
        fileMenuItem.submenu = fileMenu

        // Edit menu — primarily so ⌘C copies the page text selection. Target `self` so the action
        // reaches our `copy(_:)` even when the BitmapView (not a text control) has focus.
        let editMenuItem = NSMenuItem()
        mainMenu.addItem(editMenuItem)
        let editMenu = NSMenu(title: "Edit")
        let copyItem = NSMenuItem(title: "Copy", action: #selector(copy(_:)), keyEquivalent: "c")
        copyItem.target = self
        editMenu.addItem(copyItem)
        editMenuItem.submenu = editMenu

        // View menu with navigation shortcuts.
        let viewMenuItem = NSMenuItem()
        mainMenu.addItem(viewMenuItem)
        let viewMenu = NSMenu(title: "View")
        let openLocation = NSMenuItem(title: "Open Location", action: #selector(focusAddressBar), keyEquivalent: "l")
        openLocation.target = self
        viewMenu.addItem(openLocation)
        let reloadItem = NSMenuItem(title: "Reload Page", action: #selector(reload), keyEquivalent: "r")
        reloadItem.target = self
        viewMenu.addItem(reloadItem)
        viewMenu.addItem(NSMenuItem.separator())
        let backItem = NSMenuItem(title: "Back", action: #selector(goBack), keyEquivalent: "[")
        backItem.target = self
        viewMenu.addItem(backItem)
        let forwardItem = NSMenuItem(title: "Forward", action: #selector(goForward), keyEquivalent: "]")
        forwardItem.target = self
        viewMenu.addItem(forwardItem)
        viewMenu.addItem(NSMenuItem.separator())
        let devToolsItem = NSMenuItem(title: "Toggle DevTools", action: #selector(toggleDevTools), keyEquivalent: "i")
        devToolsItem.keyEquivalentModifierMask = [.command, .option]
        devToolsItem.target = self
        viewMenu.addItem(devToolsItem)
        viewMenuItem.submenu = viewMenu

        // Window menu (tab switching).
        let windowMenuItem = NSMenuItem()
        mainMenu.addItem(windowMenuItem)
        let windowMenu = NSMenu(title: "Window")

        let nextTab = NSMenuItem(title: "Next Tab", action: #selector(selectNextTab), keyEquivalent: "]")
        nextTab.keyEquivalentModifierMask = [.command, .shift]
        nextTab.target = self
        windowMenu.addItem(nextTab)

        let prevTab = NSMenuItem(title: "Previous Tab", action: #selector(selectPreviousTab), keyEquivalent: "[")
        prevTab.keyEquivalentModifierMask = [.command, .shift]
        prevTab.target = self
        windowMenu.addItem(prevTab)

        // Ctrl-Tab next tab (alternative). Uses a tab character key equivalent.
        let ctrlTab = NSMenuItem(title: "Cycle Tab", action: #selector(selectNextTab), keyEquivalent: "\t")
        ctrlTab.keyEquivalentModifierMask = [.control]
        ctrlTab.target = self
        ctrlTab.isAlternate = false
        windowMenu.addItem(ctrlTab)

        windowMenu.addItem(NSMenuItem.separator())

        // ⌘1…⌘9 jump to tab N.
        for n in 1...9 {
            let item = NSMenuItem(title: "Tab \(n)", action: #selector(jumpToTab(_:)), keyEquivalent: "\(n)")
            item.keyEquivalentModifierMask = [.command]
            item.tag = n - 1
            item.target = self
            windowMenu.addItem(item)
        }
        windowMenuItem.submenu = windowMenu

        NSApplication.shared.mainMenu = mainMenu
    }

    // MARK: Tab management

    /// Create a new tab with a fresh engine, make it active, and rebuild the tab bar.
    @discardableResult
    private func createTab(initialURL: String?, focusAddressBar: Bool) -> Tab {
        let tab = Tab()
        // Seed the new engine with the current OS appearance so its first cascade uses the right
        // prefers-color-scheme (rather than the default light).
        if let engine = tab.engine { browser_engine_set_color_scheme(engine, isDarkAppearance) }
        if let initialURL = initialURL, !initialURL.isEmpty {
            tab.urlString = initialURL
            tab.title = hostTitle(from: initialURL)
        }
        tabs.append(tab)
        activeIndex = tabs.count - 1
        rebuildTabBar()
        syncUIToActiveTab()
        updateViewport()
        refresh()
        if focusAddressBar {
            DispatchQueue.main.async { [weak self] in self?.focusAddressBar() }
        }
        return tab
    }

    @objc private func newTab() {
        createTab(initialURL: nil, focusAddressBar: true)
    }

    @objc private func closeCurrentTab() {
        closeTab(at: activeIndex)
    }

    private func closeTab(at index: Int) {
        guard index >= 0, index < tabs.count else { return }

        // If this is the last tab, keep a single fresh empty tab instead of crashing/closing.
        if tabs.count == 1 {
            let old = tabs[0]
            old.freeEngine()
            tabs.removeAll()
            createTab(initialURL: nil, focusAddressBar: true)
            return
        }

        let tab = tabs[index]
        tab.freeEngine() // free this engine exactly once; it's about to be dropped.
        tabs.remove(at: index)

        // Pick a neighbor as active.
        if activeIndex > index {
            activeIndex -= 1
        } else if activeIndex == index {
            activeIndex = min(index, tabs.count - 1)
        }
        rebuildTabBar()
        syncUIToActiveTab()
        updateViewport()
        refresh()
    }

    @objc private func selectNextTab() {
        guard !tabs.isEmpty else { return }
        switchToTab(at: (activeIndex + 1) % tabs.count)
    }

    @objc private func selectPreviousTab() {
        guard !tabs.isEmpty else { return }
        switchToTab(at: (activeIndex - 1 + tabs.count) % tabs.count)
    }

    @objc private func jumpToTab(_ sender: NSMenuItem) {
        let idx = sender.tag
        guard idx >= 0, idx < tabs.count else { return }
        switchToTab(at: idx)
    }

    /// Make tab `index` active: sync the address bar, nav state, viewport, and content.
    private func switchToTab(at index: Int) {
        guard index >= 0, index < tabs.count, index != activeIndex else {
            if index == activeIndex { return }
            return
        }
        activeIndex = index
        updateActiveTabHighlight()
        syncUIToActiveTab()
        updateViewport()
        // The active engine changed: reset the (panel-global) REPL session and refresh both tabs.
        devTools?.clearREPL()
        refresh()
    }

    private func rebuildTabBar() {
        for button in tabButtons {
            tabStack.removeArrangedSubview(button)
            button.removeFromSuperview()
        }
        tabButtons.removeAll()

        for tab in tabs {
            let button = TabButton(tab: tab)
            button.onSelect = { [weak self, weak tab] in
                guard let self = self, let tab = tab, let idx = self.tabs.firstIndex(where: { $0 === tab }) else { return }
                self.switchToTab(at: idx)
            }
            button.onClose = { [weak self, weak tab] in
                guard let self = self, let tab = tab, let idx = self.tabs.firstIndex(where: { $0 === tab }) else { return }
                self.closeTab(at: idx)
            }
            tabStack.addArrangedSubview(button)
            tabButtons.append(button)
        }
        updateActiveTabHighlight()
    }

    private func updateActiveTabHighlight() {
        for (i, button) in tabButtons.enumerated() {
            button.isActive = (i == activeIndex)
        }
    }

    /// Reflect the active tab's URL + nav state into the toolbar UI.
    private func syncUIToActiveTab() {
        guard let tab = activeTab else { return }
        urlField.stringValue = tab.urlString
        updateNavButtons()
        if tab.isLoading {
            progress.startAnimation(nil)
        } else {
            progress.stopAnimation(nil)
        }
    }

    private func refreshActiveTabButton() {
        guard activeIndex >= 0, activeIndex < tabButtons.count else { return }
        tabButtons[activeIndex].updateTitle()
    }

    private func hostTitle(from urlString: String) -> String {
        if let url = URL(string: urlString), let host = url.host {
            return host.hasPrefix("www.") ? String(host.dropFirst(4)) : host
        }
        let trimmed = urlString.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? "New Tab" : trimmed
    }

    // MARK: Viewport

    private func updateViewport() {
        guard let engine = activeTab?.engine, let window = window, let bitmapView = bitmapView else { return }
        let scale = Float(window.backingScaleFactor)
        let logicalWidth = UInt32(max(1, bitmapView.bounds.width.rounded()))
        let logicalHeight = UInt32(max(1, bitmapView.bounds.height.rounded()))
        browser_engine_set_viewport(engine, logicalWidth, logicalHeight, scale)
    }

    // MARK: OS appearance (prefers-color-scheme)

    /// Whether the app's effective appearance is Dark. Resolves the appearance against the standard
    /// Aqua/DarkAqua pair so auto/named appearances collapse to a simple light/dark answer.
    private var isDarkAppearance: Bool {
        let appearance = window?.effectiveAppearance ?? NSApp.effectiveAppearance
        return appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
    }

    /// Push the current OS appearance (`prefers-color-scheme`) into every tab's engine and re-render
    /// the active page so media-query-driven styles (CSS `@media` + JS `matchMedia`) restyle live.
    /// Called on launch and whenever the effective appearance changes (Light/Dark toggle).
    func applyColorScheme() {
        let dark = isDarkAppearance
        for tab in tabs {
            if let engine = tab.engine { browser_engine_set_color_scheme(engine, dark) }
        }
        refresh()
    }

    func windowDidResize(_ notification: Notification) {
        // During a live drag this fires many times/sec; each re-layout is expensive. Coalesce:
        // the BitmapView stretches the last bitmap to fit meanwhile, and we re-layout crisply
        // once the drag pauses (~40ms idle).
        bitmapView?.needsDisplay = true
        resizeWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            self?.updateViewport()
            self?.refresh()
        }
        resizeWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.04, execute: work)
    }

    func windowDidChangeBackingProperties(_ notification: Notification) {
        updateViewport()
        refresh()
    }

    // MARK: Scrolling

    /// Scroll the active tab's page by `dyPoints` (points) and re-render. The engine works
    /// in device pixels, so scale by the backing factor.
    private func scrollActiveTab(_ dyPoints: CGFloat) {
        guard let engine = activeTab?.engine else { return }
        let scale = Float(window?.backingScaleFactor ?? 1)
        browser_engine_scroll_by(engine, Float(dyPoints) * scale)
        refresh()
    }

    // MARK: Link hit-testing

    /// Map a view-local point (points, bottom-left origin because the view is NOT flipped) to the
    /// framebuffer's device-pixel coordinate space (top-left origin) and ask the engine whether a
    /// link is there. Returns the absolute URL string, or nil.
    private func linkURL(at localPoint: CGPoint) -> String? {
        guard let engine = activeTab?.engine, let bitmapView = bitmapView else { return nil }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        // The framebuffer is top-origin, the view is bottom-origin: flip y, then scale to device px.
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)
        guard let cstr = browser_engine_link_at(engine, fxDevice, fyDevice) else { return nil }
        return String(cString: cstr)
    }

    /// A click on the page content. First dispatches a `click` into the live page JS (so the
    /// page's own handlers run — interactivity); then, if the click landed on a link, navigates
    /// (recording history so Back works). If JS mutated the DOM but it wasn't a link, re-renders.
    private func handleContentClick(_ localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, let bitmapView = bitmapView else { return }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)

        // 0. If the click landed on a <select>, pop up a native dropdown menu instead of the normal
        // click handling. The engine returns the select's options + selected index + on-screen rect.
        if tab.pendingLoads == 0,
           let cstr = browser_engine_select_at(engine, fxDevice, fyDevice) {
            presentSelectMenu(json: String(cString: cstr), engine: engine, bitmapView: bitmapView, scale: scale)
            return
        }

        // 1. Fire the page's JS click handlers (bubbling). Returns 1 if the DOM changed. Skip while
        // a load is running on the engine queue (would race the background mutation).
        let changed = tab.pendingLoads == 0 ? browser_engine_dispatch_click(engine, fxDevice, fyDevice) : 0

        // 2. If it landed on a link, navigate (supersedes a re-render).
        if let cstr = browser_engine_link_at(engine, fxDevice, fyDevice) {
            let url = String(cString: cstr)
            urlField.stringValue = url
            load(urlString: url, recordHistory: true)
            refresh()
            return
        }

        // 3. If the click focused a text field, take keyboard focus so typing routes to the page.
        if browser_engine_has_text_focus(engine) != 0 {
            window?.makeFirstResponder(bitmapView)
        }

        // 4. Otherwise, repaint if the JS handler changed the page.
        if changed != 0 { refresh() }
    }

    /// The `<select>` node id the currently-open native dropdown menu belongs to (set while a menu
    /// from `presentSelectMenu` is up; consumed by `selectMenuItemChosen`).
    private var openSelectNodeID: Int = -1

    /// Build and pop up a native `NSMenu` for a `<select>` from the JSON returned by
    /// `browser_engine_select_at` (`{"id","x","y","w","h","selected","options":[...]}`). The rect is
    /// in framebuffer device px (top-origin); we convert it to the view's bottom-origin point space
    /// and pop the menu up at the control's top-left so it overlays the select. On pick, calls
    /// `browser_engine_set_select_index` and refreshes.
    private func presentSelectMenu(json: String, engine: OpaquePointer, bitmapView: NSView, scale: CGFloat) {
        guard let data = json.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = obj["id"] as? Int,
              let options = obj["options"] as? [String] else { return }
        let selected = (obj["selected"] as? Int) ?? 0
        let rx = (obj["x"] as? Double).map { CGFloat($0) } ?? 0
        let ry = (obj["y"] as? Double).map { CGFloat($0) } ?? 0

        self.openSelectNodeID = id
        let menu = NSMenu()
        menu.autoenablesItems = false
        for (i, label) in options.enumerated() {
            let item = NSMenuItem(title: label, action: #selector(selectMenuItemChosen(_:)), keyEquivalent: "")
            item.target = self
            item.tag = i
            item.state = (i == selected) ? .on : .off
            menu.addItem(item)
        }

        // Engine rect is device px, top-origin. Convert to view points; the view is NOT flipped
        // (bottom-left origin), so the select's TOP edge in view space is (height - ry/scale). Pop
        // the menu up there so it overlays the control.
        let xPts = rx / scale
        let topInViewPts = bitmapView.bounds.height - (ry / scale)
        let point = CGPoint(x: xPts, y: topInViewPts)
        let preselected = (selected >= 0 && selected < menu.items.count) ? menu.items[selected] : nil
        menu.popUp(positioning: preselected, at: point, in: bitmapView)
    }

    /// A native dropdown menu item was chosen: apply the selection to the engine's `<select>` and
    /// re-render if it changed.
    @objc private func selectMenuItemChosen(_ sender: NSMenuItem) {
        guard let engine = activeTab?.engine, openSelectNodeID >= 0 else { return }
        let index = sender.tag
        let changed = browser_engine_set_select_index(engine, UInt(openSelectNodeID), UInt(index))
        openSelectNodeID = -1
        if changed != 0 { refresh() }
    }

    /// A raw mouse event (mousedown/mouseup/dblclick/contextmenu) on the page content.
    private func handleMouseEvent(_ kind: String, _ localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, let bitmapView = bitmapView,
              tab.pendingLoads == 0 else { return }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)
        let changed = kind.withCString { browser_engine_dispatch_mouse(engine, $0, fxDevice, fyDevice) }
        if changed != 0 { refresh() }
    }

    /// Pointer moved over the page: fire the page's hover events (mouseover/out/enter/leave). The
    /// engine no-ops cheaply when the hovered node is unchanged; we only repaint on a real change.
    private func handleContentMove(_ localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, let bitmapView = bitmapView,
              tab.pendingLoads == 0 else { return }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)
        if browser_engine_dispatch_move(engine, fxDevice, fyDevice) != 0 { refresh() }
    }

    // MARK: Text selection

    /// Convert a view-local point (points, bottom-left origin) to framebuffer device-pixel
    /// (top-left origin) coords — the SAME viewport-relative, pre-scroll space `handleContentClick`
    /// passes to the engine. The engine folds in its own `scroll_y` for selection, so the highlight
    /// stays under the cursor as the page scrolls.
    private func deviceCoords(_ localPoint: CGPoint) -> (Float, Float)? {
        guard let bitmapView = bitmapView else { return nil }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        return (Float(localPoint.x * scale), Float(fyTop * scale))
    }

    /// Mouse-down: set the selection anchor (collapsed) at the press point. A real selection only
    /// appears once the pointer drags (extend); a plain click clears it via `handleSelectCancel`.
    private func handleSelectStart(_ localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0,
              let (x, y) = deviceCoords(localPoint) else { return }
        browser_engine_selection_start(engine, x, y)
    }

    /// Extend the selection focus to the current pointer point and repaint so the highlight updates
    /// live during the drag (also used as the drag-end handler — one final extend).
    private func handleSelectExtend(_ localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0,
              let (x, y) = deviceCoords(localPoint) else { return }
        browser_engine_selection_extend(engine, x, y)
        refresh()
    }

    /// A plain click (no drag): clear any selection and repaint so the old highlight disappears.
    private func handleSelectCancel() {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0 else { return }
        // Only repaint if there was actually a selection to clear (avoid churning every click).
        let had = browser_engine_has_selection(engine) != 0
        browser_engine_selection_clear(engine)
        if had { refresh() }
    }

    /// Copy the current page text selection to the pasteboard (⌘C). Returns true if it copied.
    @discardableResult
    private func copySelectionToPasteboard() -> Bool {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0,
              browser_engine_has_selection(engine) != 0,
              let cstr = browser_engine_selected_text(engine) else { return false }
        let text = String(cString: cstr)
        guard !text.isEmpty else { return false }
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
        return true
    }

    /// ⌘C from the Edit menu: copy the page selection if there is one. When a text control (URL
    /// field, devtools console/REPL) holds focus, defer to its own copy so selecting-and-copying in
    /// those fields keeps working.
    @objc private func copy(_ sender: Any?) {
        if let responder = window?.firstResponder, responder !== bitmapView,
           responder is NSText || responder is NSTextView || responder is NSTextField {
            responder.tryToPerform(#selector(NSText.copy(_:)), with: sender)
            return
        }
        if copySelectionToPasteboard() { return }
        // No page selection: let the focused responder (if any) handle a normal copy.
        window?.firstResponder?.tryToPerform(#selector(NSText.copy(_:)), with: sender)
    }

    /// Route a key event to the focused page text field. Returns true if consumed. Lets anything
    /// with a Command modifier (menu shortcuts) propagate, and only acts when a field is focused.
    private func handleKeyDown(_ event: NSEvent) -> Bool {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0 else { return false }
        if event.modifierFlags.contains(.command) { return false }
        guard browser_engine_has_text_focus(engine) != 0 else { return false }

        // Map the AppKit key event to a DOM key name + a rough physical code.
        let (key, code) = Self.domKey(for: event)
        guard !key.isEmpty else { return false }

        let changed = key.withCString { k in code.withCString { c in
            browser_engine_dispatch_key(engine, k, c)
        } }
        if changed != 0 { refresh() }
        return true
    }

    /// Translate an NSEvent into a (DOM `key`, DOM `code`) pair.
    private static func domKey(for event: NSEvent) -> (String, String) {
        switch event.keyCode {
        case 51: return ("Backspace", "Backspace")
        case 117: return ("Delete", "Delete")
        case 36, 76: return ("Enter", "Enter")
        case 48: return ("Tab", "Tab")
        case 53: return ("Escape", "Escape")
        case 123: return ("ArrowLeft", "ArrowLeft")
        case 124: return ("ArrowRight", "ArrowRight")
        case 125: return ("ArrowDown", "ArrowDown")
        case 126: return ("ArrowUp", "ArrowUp")
        case 49: return (" ", "Space")
        default:
            // Printable characters: use what the keyboard produced (respects shift/layout).
            let chars = event.characters ?? ""
            if let scalar = chars.unicodeScalars.first, scalar.value >= 0x20, chars.count == 1 {
                let ignoring = (event.charactersIgnoringModifiers ?? chars).uppercased()
                let code: String
                if let c = ignoring.first, c.isLetter { code = "Key\(c)" }
                else if let c = ignoring.first, c.isNumber { code = "Digit\(c)" }
                else { code = "" }
                return (chars, code)
            }
            return ("", "")
        }
    }

    // MARK: DevTools

    @objc private func toggleDevTools() {
        devToolsVisible.toggle()
        devTools.isHidden = !devToolsVisible
        // Swap which bitmap-bottom constraint is active so the bitmap shrinks/grows.
        bitmapBottomToContent.isActive = !devToolsVisible
        bitmapBottomToDevTools.isActive = devToolsVisible
        window.layoutIfNeeded()
        // The bitmap changed size: re-layout the page at the new viewport.
        updateViewport()
        refresh()
        if devToolsVisible {
            devTools.refreshVisible()
            devTools.focusREPL()
        } else {
            // Returning focus to the page lets page typing work again.
            window.makeFirstResponder(bitmapView)
        }
    }

    /// Resize the DevTools panel from a top-edge drag (`delta` points, positive = grow taller).
    /// Clamped so neither the panel nor the page area collapses.
    private func resizeDevTools(by delta: CGFloat) {
        guard devToolsVisible, let content = window.contentView else { return }
        let maxH = max(140, content.bounds.height - 160) // leave room for the page + toolbar
        let newH = min(max(devToolsHeightConstraint.constant + delta, 120), maxH)
        guard abs(newH - devToolsHeightConstraint.constant) > 0.5 else { return }
        devToolsHeightConstraint.constant = newH
        window.layoutIfNeeded()
        updateViewport()
        refresh()
    }

    /// The view-local point of the most recent right-click (so the menu's Inspect/Paste actions,
    /// which carry no point, can act where the user clicked).
    private var lastContextPoint: CGPoint = .zero

    /// Build the page right-click menu: Copy (if a selection exists), Paste (if a text field is
    /// focused + the clipboard has text), Inspect Element, and Back/Forward/Reload.
    private func buildContextMenu(at localPoint: CGPoint) -> NSMenu {
        lastContextPoint = localPoint
        let menu = NSMenu()
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0 else { return menu }

        let canCopy = browser_engine_has_selection(engine) != 0
        let copyItem = NSMenuItem(title: "Copy", action: #selector(ctxCopy), keyEquivalent: "")
        copyItem.target = self; copyItem.isEnabled = canCopy
        menu.addItem(copyItem)

        let canPaste = browser_engine_has_text_focus(engine) != 0 && NSPasteboard.general.string(forType: .string) != nil
        let pasteItem = NSMenuItem(title: "Paste", action: #selector(ctxPaste), keyEquivalent: "")
        pasteItem.target = self; pasteItem.isEnabled = canPaste
        menu.addItem(pasteItem)

        menu.addItem(.separator())
        let backItem = NSMenuItem(title: "Back", action: #selector(goBack), keyEquivalent: "")
        backItem.target = self; backItem.isEnabled = tab.canGoBack
        menu.addItem(backItem)
        let fwdItem = NSMenuItem(title: "Forward", action: #selector(goForward), keyEquivalent: "")
        fwdItem.target = self; fwdItem.isEnabled = tab.canGoForward
        menu.addItem(fwdItem)
        let reloadItem = NSMenuItem(title: "Reload", action: #selector(reload), keyEquivalent: "")
        reloadItem.target = self
        menu.addItem(reloadItem)

        menu.addItem(.separator())
        let inspectItem = NSMenuItem(title: "Inspect Element", action: #selector(ctxInspect), keyEquivalent: "")
        inspectItem.target = self
        menu.addItem(inspectItem)
        return menu
    }

    @objc private func ctxCopy() { copySelectionToPasteboard() }
    @objc private func ctxInspect() { inspectElement(at: lastContextPoint) }

    /// Paste clipboard text into the focused page text field by dispatching each character as a key.
    @objc private func ctxPaste() {
        guard let tab = activeTab, let engine = tab.engine, tab.pendingLoads == 0,
              browser_engine_has_text_focus(engine) != 0,
              let str = NSPasteboard.general.string(forType: .string), !str.isEmpty else { return }
        var changed = false
        for ch in str where ch != "\n" && ch != "\r" {
            let s = String(ch)
            let did = s.withCString { k in s.withCString { c in browser_engine_dispatch_key(engine, k, c) } }
            if did != 0 { changed = true }
        }
        if changed { refresh() }
    }

    /// Open DevTools (if hidden) on the Elements tab and inspect the element at a view-local point —
    /// the "Inspect Element" entry point (called from the right-click menu).
    func inspectElement(at localPoint: CGPoint) {
        guard let tab = activeTab, let engine = tab.engine, let bitmapView = bitmapView,
              tab.pendingLoads == 0 else { return }
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)
        let nodeId = browser_engine_node_at_point(engine, fxDevice, fyDevice)
        guard nodeId >= 0 else { return }
        if !devToolsVisible { toggleDevTools() }
        devTools.inspect(nodeId: Int(nodeId))
    }

    // MARK: Rendering

    func refresh() {
        guard let engine = activeTab?.engine else { return }
        let fb = browser_engine_render(engine)
        guard let pixels = fb.pixels else { return }
        let data = Data(bytes: pixels, count: Int(fb.stride * fb.height)) // copy; ptr valid until next render
        setBitmapImage(data: data, width: Int(fb.width), height: Int(fb.height), stride: Int(fb.stride))

        // Refresh the visible devtools tab on the render path (console text / network entries
        // accumulate during load + async ticks). Guarded internally to be cheap when hidden.
        devTools?.refreshVisible()
    }

    /// Build a CGImage from an already-copied RGBA buffer and show it in the bitmap view. Shared by
    /// the pull render (`refresh`) and the streaming progress callback (progressive first paint).
    func setBitmapImage(data: Data, width: Int, height: Int, stride: Int) {
        guard width > 0, height > 0, let provider = CGDataProvider(data: data as CFData) else { return }
        let bitmapInfo = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue)
        let image = CGImage(
            width: width, height: height,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: stride,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: bitmapInfo,
            provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent
        )
        bitmapView.image = image
        bitmapView.setNeedsDisplay(bitmapView.bounds)
    }

    /// A progressive frame painted by the engine WHILE a page streams in (pushed from the load
    /// thread via the C callback). Only show it if `tab` is still the visible tab.
    func displayProgressFrame(forTab tab: Tab, data: Data, width: Int, height: Int, stride: Int) {
        guard tab === activeTab else { return }
        setBitmapImage(data: data, width: width, height: height, stride: stride)
    }

    // MARK: Navigation

    private func normalize(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return trimmed }
        if trimmed.contains("://") { return trimmed }
        // A bare host like "example.com" -> "https://example.com".
        return "https://" + trimmed
    }

    @objc private func navigate(_ sender: Any?) {
        let url = normalize(urlField.stringValue)
        guard !url.isEmpty else { return }
        urlField.stringValue = url
        load(urlString: url, recordHistory: true)
    }

    @objc private func goBack() {
        guard let tab = activeTab, tab.canGoBack else { return }
        tab.historyIndex -= 1
        let url = tab.history[tab.historyIndex]
        urlField.stringValue = url
        load(urlString: url, recordHistory: false)
        updateNavButtons()
    }

    @objc private func goForward() {
        guard let tab = activeTab, tab.canGoForward else { return }
        tab.historyIndex += 1
        let url = tab.history[tab.historyIndex]
        urlField.stringValue = url
        load(urlString: url, recordHistory: false)
        updateNavButtons()
    }

    @objc private func reload() {
        guard let tab = activeTab else { return }
        if tab.historyIndex >= 0, tab.historyIndex < tab.history.count {
            load(urlString: tab.history[tab.historyIndex], recordHistory: false)
        } else {
            let url = normalize(urlField.stringValue)
            if !url.isEmpty { load(urlString: url, recordHistory: true) }
        }
    }

    @objc private func focusAddressBar() {
        window.makeFirstResponder(urlField)
        urlField.currentEditor()?.selectAll(nil)
    }

    private func updateNavButtons() {
        guard let tab = activeTab else {
            backButton.isEnabled = false
            forwardButton.isEnabled = false
            reloadButton.isEnabled = false
            return
        }
        backButton.isEnabled = tab.canGoBack
        forwardButton.isEnabled = tab.canGoForward
        reloadButton.isEnabled = tab.historyIndex >= 0 || !urlField.stringValue.isEmpty
    }

    /// Load a URL on a background queue (single engine access at a time), then refresh on main.
    /// The load is bound to a specific tab so a background switch can't corrupt another engine.
    private func load(urlString: String, recordHistory shouldRecord: Bool) {
        guard let tab = activeTab, let engine = tab.engine else { return }
        let urlCopy = urlString

        // A REPL session is per page: navigating starts a fresh one.
        devTools?.clearREPL()

        if shouldRecord {
            tab.recordHistory(urlString)
        }
        tab.urlString = urlString
        tab.title = hostTitle(from: urlString)
        refreshActiveTabButton()
        updateNavButtons()

        // Start the spinner before dispatching.
        tab.isLoading = true
        tab.pendingLoads += 1
        inFlightLoads += 1
        progress.startAnimation(nil)
        // This navigation supersedes any earlier in-flight one on this tab.
        tab.loadGeneration += 1
        let generation = tab.loadGeneration

        // Run on the tab's SERIAL engine queue: loads never overlap on one engine, and they apply
        // in navigation order. A superseded load (generation mismatch) still runs to completion
        // but does not touch the UI.
        tab.engineQueue.async { [weak self] in
            // `tab` is captured strongly so the engine stays alive for the whole call;
            // closeTab() defers the actual free until pendingLoads drains (see freeEngine()).
            _ = urlCopy.withCString { cstr in
                browser_engine_load_url(engine, cstr)
            }
            DispatchQueue.main.async {
                tab.isLoading = false
                tab.pendingLoads -= 1
                // If the tab was closed while loading, free its engine now that it's idle.
                if tab.pendingLoads == 0 && tab.freeWhenIdle {
                    tab.freeEngine()
                }
                guard let self = self else { return }
                self.inFlightLoads -= 1
                if self.inFlightLoads <= 0 {
                    self.inFlightLoads = 0
                    self.progress.stopAnimation(nil)
                }
                // A newer navigation has superseded this one: don't clobber its title/render.
                if tab.loadGeneration != generation { return }
                // Use the page's <title> for the tab label (fall back to the host title).
                if let cstr = browser_engine_title(engine) {
                    let pageTitle = String(cString: cstr)
                    if !pageTitle.isEmpty { tab.title = pageTitle }
                }
                if let idx = self.tabs.firstIndex(where: { $0 === tab }), idx < self.tabButtons.count {
                    self.tabButtons[idx].updateTitle()
                }
                // Only repaint if the tab that finished is still the active one.
                if self.activeTab === tab {
                    self.refresh()
                    self.updateNavButtons()
                }
            }
        }
    }
}

// MARK: - Entry point

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = AppDelegate()
app.delegate = delegate
app.activate(ignoringOtherApps: true)
app.run()
