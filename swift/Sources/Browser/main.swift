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
        mouseDownPoint = convert(event.locationInWindow, from: nil)
    }

    override func mouseUp(with event: NSEvent) {
        let up = convert(event.locationInWindow, from: nil)
        defer { mouseDownPoint = nil }
        // Treat as a click only if the pointer barely moved (not a drag / text selection).
        if let down = mouseDownPoint {
            let dx = up.x - down.x
            let dy = up.y - down.y
            if (dx * dx + dy * dy) > 16 { return } // moved > 4pt: a drag, ignore
        }
        onClick?(up)
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

    init() {
        engine = browser_engine_new()
    }

    /// Free the engine. Safe to call multiple times; subsequent calls are no-ops.
    /// If a background load is in flight, defers the free until that load completes.
    func freeEngine() {
        if pendingLoads > 0 {
            freeWhenIdle = true
            return
        }
        if let engine = engine {
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
        toolTip = tab.urlString.isEmpty ? tab.title : tab.urlString
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

// MARK: - AppDelegate

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    var urlField: URLTextField!
    var bitmapView: BitmapView!

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
        window.center()

        let content = NSView(frame: contentRect)
        content.wantsLayer = true
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
        content.addSubview(bitmapView)

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
            bitmapView.bottomAnchor.constraint(equalTo: content.bottomAnchor),

            // Nav buttons pinned to the leading edge, clear of the traffic lights.
            navStack.leadingAnchor.constraint(equalTo: toolbar.leadingAnchor, constant: 80),
            navStack.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),

            // Spinner trails the address bar on the right.
            progress.trailingAnchor.constraint(equalTo: toolbar.trailingAnchor, constant: -16),
            progress.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            progress.widthAnchor.constraint(equalToConstant: 16),
            progress.heightAnchor.constraint(equalToConstant: 16),

            // Pill: centered, expands with sensible margins + max width.
            pill.centerXAnchor.constraint(equalTo: toolbar.centerXAnchor),
            pill.centerYAnchor.constraint(equalTo: toolbar.centerYAnchor),
            pill.heightAnchor.constraint(equalToConstant: 32),

            lockSymbol.leadingAnchor.constraint(equalTo: pill.leadingAnchor, constant: 12),
            lockSymbol.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
            lockSymbol.widthAnchor.constraint(equalToConstant: 14),

            urlField.leadingAnchor.constraint(equalTo: lockSymbol.trailingAnchor, constant: 8),
            urlField.trailingAnchor.constraint(equalTo: pill.trailingAnchor, constant: -14),
            urlField.centerYAnchor.constraint(equalTo: pill.centerYAnchor),
        ])

        // Pill width: grows with the window but capped, and never overlaps nav/spinner.
        let pillMaxWidth = pill.widthAnchor.constraint(lessThanOrEqualToConstant: 640)
        pillMaxWidth.priority = .required
        let pillIdealWidth = pill.widthAnchor.constraint(equalTo: toolbar.widthAnchor, multiplier: 0.55)
        pillIdealWidth.priority = .defaultHigh
        let pillLeadingGap = pill.leadingAnchor.constraint(greaterThanOrEqualTo: navStack.trailingAnchor, constant: 16)
        let pillTrailingGap = pill.trailingAnchor.constraint(lessThanOrEqualTo: progress.leadingAnchor, constant: -16)
        // Min width is non-required so the pill compresses on narrow windows instead of
        // blocking horizontal resize (the leading/trailing gaps still prevent overlap).
        let pillMinWidth = pill.widthAnchor.constraint(greaterThanOrEqualToConstant: 90)
        pillMinWidth.priority = .defaultLow
        NSLayoutConstraint.activate([pillMaxWidth, pillIdealWidth, pillLeadingGap, pillTrailingGap, pillMinWidth])

        // Only listen for resize/backing callbacks once all views exist, so an early
        // notification can't reach updateViewport() before bitmapView is set.
        window.delegate = self
        window.makeKeyAndOrderFront(nil)

        // Create the first tab (becomes active) and start loading the default URL.
        createTab(initialURL: defaultURL, focusAddressBar: false)
        updateViewport()
        refresh()
        if let url = activeTab?.urlString, !url.isEmpty {
            load(urlString: url, recordHistory: true)
        }
    }

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

    /// A simple click on the page content: if it landed on a link, navigate to it (recording
    /// history so Back works), reusing the same path as entering a URL.
    private func handleContentClick(_ localPoint: CGPoint) {
        guard let url = linkURL(at: localPoint) else { return }
        urlField.stringValue = url
        load(urlString: url, recordHistory: true)
        refresh()
    }

    // MARK: Rendering

    func refresh() {
        guard let engine = activeTab?.engine else { return }
        let fb = browser_engine_render(engine)
        guard fb.pixels != nil else { return }

        let count = Int(fb.stride * fb.height)
        let data = Data(bytes: fb.pixels!, count: count) // copy out; pointer only valid until next render
        guard let provider = CGDataProvider(data: data as CFData) else { return }
        let bitmapInfo = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue)
        let image = CGImage(
            width: Int(fb.width),
            height: Int(fb.height),
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: Int(fb.stride),
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: bitmapInfo,
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        )
        bitmapView.image = image
        bitmapView.setNeedsDisplay(bitmapView.bounds)
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

        DispatchQueue.global().async { [weak self] in
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
