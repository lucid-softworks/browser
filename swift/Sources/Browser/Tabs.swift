import AppKit
import CBrowser

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
    /// The page's decoded favicon (from the engine), shown in the tab chip + address bar. `nil`
    /// until one loads (or for pages without an icon).
    var favicon: NSImage?

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
    private let faviconView = NSImageView()
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

        // Layout (Chrome/Firefox order): favicon on the left, title in the middle, close "×" on the
        // right (the close button only appears on hover/active; its slot is reserved either way so
        // the title doesn't shift).
        faviconView.imageScaling = .scaleProportionallyDown
        faviconView.translatesAutoresizingMaskIntoConstraints = false
        addSubview(faviconView)

        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 28),
            widthAnchor.constraint(greaterThanOrEqualToConstant: 90),
            widthAnchor.constraint(lessThanOrEqualToConstant: 200),

            faviconView.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 9),
            faviconView.centerYAnchor.constraint(equalTo: centerYAnchor),
            faviconView.widthAnchor.constraint(equalToConstant: 16),
            faviconView.heightAnchor.constraint(equalToConstant: 16),

            closeButton.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            closeButton.centerYAnchor.constraint(equalTo: centerYAnchor),
            closeButton.widthAnchor.constraint(equalToConstant: 18),
            closeButton.heightAnchor.constraint(equalToConstant: 18),

            titleLabel.leadingAnchor.constraint(equalTo: faviconView.trailingAnchor, constant: 7),
            titleLabel.trailingAnchor.constraint(equalTo: closeButton.leadingAnchor, constant: -4),
            titleLabel.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])

        updateTitle()
        updateAppearance()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    func updateTitle() {
        titleLabel.stringValue = tab.title.isEmpty ? "New Tab" : tab.title
        faviconView.image = tab.favicon
        updateAppearance() // favicon presence affects the leading-slot visibility
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
        // Close "×" (right) shows only on active/hover; favicon (left) shows whenever there is one.
        closeButton.isHidden = !(isActive || hovering)
        faviconView.isHidden = tab.favicon == nil
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

