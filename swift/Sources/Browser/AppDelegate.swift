import AppKit
import CBrowser

// MARK: - AppDelegate

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    var urlField: URLTextField!
    var bitmapView: BitmapView!

    // Hovered-link status overlay (bottom-left of the content, like Chrome/Firefox).
    private var statusBar: NSView!
    private var statusBarLabel: NSTextField!

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
    // Views + constraints whose arrangement depends on the configurable tab-strip position.
    private var toolbar: NSView!
    private var tabBar: NSView!
    private var separator: NSView!
    private var tabLayoutConstraints: [NSLayoutConstraint] = []
    private let tabColumnWidth: CGFloat = 200

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

    // The home page / new-tab URL, read live from user settings (editable via the Settings window).
    private var defaultURL: String { Config.shared.homepage }
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
        let toolbarView = NSVisualEffectView()
        toolbarView.material = .titlebar
        toolbarView.blendingMode = .withinWindow
        toolbarView.state = .followsWindowActiveState
        toolbarView.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(toolbarView)
        toolbar = toolbarView

        // MARK: Tab bar (slim translucent strip; placed at the configured edge of the page)
        let tabBarView = NSVisualEffectView()
        tabBarView.material = .headerView
        tabBarView.blendingMode = .withinWindow
        tabBarView.state = .followsWindowActiveState
        tabBarView.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(tabBarView)
        tabBar = tabBarView

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

        // A subtle hairline separator between the tab bar and the page area.
        let separatorBox = NSBox()
        separatorBox.boxType = .custom
        separatorBox.borderWidth = 0
        separatorBox.fillColor = NSColor.separatorColor
        separatorBox.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(separatorBox)
        separator = separatorBox

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
        bitmapView.onMouseExit = { [weak self] in self?.updateStatusBar(link: nil) }
        bitmapView.onMouseEvent = { [weak self] kind, point in self?.handleMouseEvent(kind, point) }
        bitmapView.onSelectStart = { [weak self] point in self?.handleSelectStart(point) }
        bitmapView.onSelectExtend = { [weak self] point in self?.handleSelectExtend(point) }
        bitmapView.onSelectEnd = { [weak self] point in self?.handleSelectExtend(point) }
        bitmapView.onSelectCancel = { [weak self] in self?.handleSelectCancel() }
        bitmapView.contextMenuProvider = { [weak self] point in self?.buildContextMenu(at: point) }
        content.addSubview(bitmapView)

        // MARK: Hovered-link status overlay (bottom-left, shown only while hovering a link)
        statusBar = NSView()
        statusBar.translatesAutoresizingMaskIntoConstraints = false
        statusBar.wantsLayer = true
        statusBar.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        statusBar.layer?.cornerRadius = 4
        statusBar.layer?.cornerCurve = .continuous
        statusBar.layer?.borderWidth = 1
        statusBar.layer?.borderColor = NSColor.separatorColor.cgColor
        statusBar.isHidden = true
        statusBarLabel = NSTextField(labelWithString: "")
        statusBarLabel.translatesAutoresizingMaskIntoConstraints = false
        statusBarLabel.font = NSFont.systemFont(ofSize: 11)
        statusBarLabel.textColor = NSColor.secondaryLabelColor
        statusBarLabel.lineBreakMode = .byTruncatingMiddle
        statusBarLabel.maximumNumberOfLines = 1
        statusBar.addSubview(statusBarLabel)
        content.addSubview(statusBar, positioned: .above, relativeTo: bitmapView)

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

        // DevTools sits below the bitmap, splitting the page area vertically. Its constant height
        // lives here; the bitmap-bottom toggle and DevTools edges are positional (rebuilt by
        // applyTabLayout for the current tab-strip position; toggleDevTools flips which bitmap-bottom
        // constraint is active).
        devToolsHeightConstraint = devTools.heightAnchor.constraint(equalToConstant: devToolsHeight)
        devToolsHeightConstraint.isActive = true

        // Status overlay: bottom-left of the bitmap, label inset by a few px, width-capped so a long
        // URL truncates instead of spanning the window.
        NSLayoutConstraint.activate([
            statusBar.leadingAnchor.constraint(equalTo: bitmapView.leadingAnchor),
            statusBar.bottomAnchor.constraint(equalTo: bitmapView.bottomAnchor),
            statusBar.widthAnchor.constraint(lessThanOrEqualTo: bitmapView.widthAnchor, multiplier: 0.8),
            statusBarLabel.leadingAnchor.constraint(equalTo: statusBar.leadingAnchor, constant: 8),
            statusBarLabel.trailingAnchor.constraint(equalTo: statusBar.trailingAnchor, constant: -8),
            statusBarLabel.topAnchor.constraint(equalTo: statusBar.topAnchor, constant: 3),
            statusBarLabel.bottomAnchor.constraint(equalTo: statusBar.bottomAnchor, constant: -3),
        ])

        // Place the tab strip at the configured edge (top/bottom/left/right) and lay out the page
        // region; re-applied live when the setting changes.
        applyTabLayout(Config.shared.tabPosition)
        Config.shared.onTabPositionChange = { [weak self] pos in self?.applyTabLayout(pos) }

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
        let settingsItem = NSMenuItem(title: "Settings…", action: #selector(openSettings), keyEquivalent: ",")
        settingsItem.target = self
        appMenu.addItem(settingsItem)
        appMenu.addItem(NSMenuItem.separator())
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
        // A tab always starts on a real document; with no explicit URL it opens `about:blank` (the
        // empty initial document), so new tabs / the last-tab-closed replacement are scriptable and
        // render a clean blank page rather than nothing.
        let resolved = initialURL.flatMap { $0.isEmpty ? nil : $0 } ?? "about:blank"
        tab.urlString = resolved
        tab.title = hostTitle(from: resolved)
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

    private var settingsWindowController: SettingsWindowController?

    @objc private func openSettings() {
        if settingsWindowController == nil {
            settingsWindowController = SettingsWindowController(currentURLProvider: { [weak self] in
                self?.activeTab?.urlString
            })
        }
        settingsWindowController?.showWindow(nil)
        settingsWindowController?.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
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
        // Show an empty address bar for the blank initial document (a "New Tab"), so the user can
        // just type — rather than displaying the literal "about:blank".
        urlField.stringValue = (tab.urlString == "about:blank" || tab.urlString == "about:") ? "" : tab.urlString
        updateAddressBarIcon()
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
        // The empty initial document presents as a "New Tab", not the literal "about:blank".
        if urlString == "about:blank" || urlString == "about:" { return "New Tab" }
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

        // 1. Resolve a link under the point FIRST: dispatching the click invalidates the layout
        //    cache that link hit-testing relies on, so link_at must read it beforehand.
        let linkTarget: String? = browser_engine_link_at(engine, fxDevice, fyDevice).map { String(cString: $0) }

        // 2. Fire the page's JS click handlers (bubbling). Returns 1 if the DOM changed. Skip while
        // a load is running on the engine queue (would race the background mutation).
        let changed = tab.pendingLoads == 0 ? browser_engine_dispatch_click(engine, fxDevice, fyDevice) : 0

        // 3. If it landed on a link, navigate (supersedes a re-render).
        if let url = linkTarget {
            updateStatusBar(link: nil)
            urlField.stringValue = url
            load(urlString: url, recordHistory: true)
            return
        }

        // 4. If the click focused a text field, take keyboard focus so typing routes to the page.
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
        // Resolve the hovered link BEFORE dispatch_move (which can invalidate the layout cache that
        // link hit-testing relies on), and show it in the status overlay.
        updateStatusBar(link: linkURL(at: localPoint))
        let scale = CGFloat(window?.backingScaleFactor ?? 1)
        let fyTop = bitmapView.bounds.height - localPoint.y
        let fxDevice = Float(localPoint.x * scale)
        let fyDevice = Float(fyTop * scale)
        if browser_engine_dispatch_move(engine, fxDevice, fyDevice) != 0 { refresh() }
    }

    /// Show `link` in the bottom-left status overlay, or hide it when there's no hovered link.
    private func updateStatusBar(link: String?) {
        if let link = link, !link.isEmpty {
            statusBarLabel.stringValue = link
            statusBar.isHidden = false
        } else {
            statusBar.isHidden = true
            statusBarLabel.stringValue = ""
        }
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
        // Otherwise copy the page's text selection (a no-op if nothing is selected). We must NOT
        // forward `copy:` up the responder chain here: the app delegate is itself in that chain, so
        // it would route straight back into this method and recurse until the stack overflows.
        copySelectionToPasteboard()
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

    /// Arrange the tab strip at the configured edge and lay out the page region (bitmap + DevTools)
    /// in the remaining space. Idempotent + live: deactivates the previous positional constraints and
    /// rebuilds them, so it can be called again when the setting changes. The toolbar (URL bar) stays
    /// at the top in every layout.
    private func applyTabLayout(_ pos: TabPosition) {
        guard let content = window?.contentView else { return }
        NSLayoutConstraint.deactivate(tabLayoutConstraints)
        tabLayoutConstraints.removeAll()
        bitmapBottomToContent?.isActive = false
        bitmapBottomToDevTools?.isActive = false

        let vertical = (pos == .left || pos == .right)
        tabStack.orientation = vertical ? .vertical : .horizontal
        tabStack.alignment = vertical ? .leading : .centerY

        var cs: [NSLayoutConstraint] = []
        // Anchors bounding the page region (bitmap + DevTools), set per position below.
        let pageTop: NSLayoutYAxisAnchor
        let pageBottom: NSLayoutYAxisAnchor
        let pageLeading: NSLayoutXAxisAnchor
        let pageTrailing: NSLayoutXAxisAnchor

        switch pos {
        case .top:
            cs += [
                tabBar.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
                tabBar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
                tabBar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
                tabBar.heightAnchor.constraint(equalToConstant: tabBarHeight),
                separator.topAnchor.constraint(equalTo: tabBar.bottomAnchor),
                separator.leadingAnchor.constraint(equalTo: content.leadingAnchor),
                separator.trailingAnchor.constraint(equalTo: content.trailingAnchor),
                separator.heightAnchor.constraint(equalToConstant: 1),
            ]
            pageTop = separator.bottomAnchor
            pageBottom = content.bottomAnchor
            pageLeading = content.leadingAnchor
            pageTrailing = content.trailingAnchor
        case .bottom:
            cs += [
                tabBar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
                tabBar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
                tabBar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
                tabBar.heightAnchor.constraint(equalToConstant: tabBarHeight),
                separator.bottomAnchor.constraint(equalTo: tabBar.topAnchor),
                separator.leadingAnchor.constraint(equalTo: content.leadingAnchor),
                separator.trailingAnchor.constraint(equalTo: content.trailingAnchor),
                separator.heightAnchor.constraint(equalToConstant: 1),
            ]
            pageTop = toolbar.bottomAnchor
            pageBottom = separator.topAnchor
            pageLeading = content.leadingAnchor
            pageTrailing = content.trailingAnchor
        case .left:
            cs += [
                tabBar.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
                tabBar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
                tabBar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
                tabBar.widthAnchor.constraint(equalToConstant: tabColumnWidth),
                separator.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
                separator.leadingAnchor.constraint(equalTo: tabBar.trailingAnchor),
                separator.bottomAnchor.constraint(equalTo: content.bottomAnchor),
                separator.widthAnchor.constraint(equalToConstant: 1),
            ]
            pageTop = toolbar.bottomAnchor
            pageBottom = content.bottomAnchor
            pageLeading = separator.trailingAnchor
            pageTrailing = content.trailingAnchor
        case .right:
            cs += [
                tabBar.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
                tabBar.trailingAnchor.constraint(equalTo: content.trailingAnchor),
                tabBar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
                tabBar.widthAnchor.constraint(equalToConstant: tabColumnWidth),
                separator.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
                separator.trailingAnchor.constraint(equalTo: tabBar.leadingAnchor),
                separator.bottomAnchor.constraint(equalTo: content.bottomAnchor),
                separator.widthAnchor.constraint(equalToConstant: 1),
            ]
            pageTop = toolbar.bottomAnchor
            pageBottom = content.bottomAnchor
            pageLeading = content.leadingAnchor
            pageTrailing = separator.leadingAnchor
        }

        // Tab list + "new tab" button inside the strip (row for top/bottom, column for left/right).
        if vertical {
            cs += [
                tabStack.topAnchor.constraint(equalTo: tabBar.topAnchor, constant: 8),
                tabStack.leadingAnchor.constraint(equalTo: tabBar.leadingAnchor, constant: 6),
                tabStack.trailingAnchor.constraint(equalTo: tabBar.trailingAnchor, constant: -6),
                tabStack.bottomAnchor.constraint(lessThanOrEqualTo: newTabButton.topAnchor, constant: -6),
                newTabButton.topAnchor.constraint(equalTo: tabStack.bottomAnchor, constant: 6),
                newTabButton.centerXAnchor.constraint(equalTo: tabBar.centerXAnchor),
                newTabButton.widthAnchor.constraint(equalToConstant: 26),
                newTabButton.heightAnchor.constraint(equalToConstant: 24),
            ]
        } else {
            cs += [
                tabStack.leadingAnchor.constraint(equalTo: tabBar.leadingAnchor, constant: 8),
                tabStack.centerYAnchor.constraint(equalTo: tabBar.centerYAnchor),
                tabStack.trailingAnchor.constraint(lessThanOrEqualTo: newTabButton.leadingAnchor, constant: -6),
                newTabButton.leadingAnchor.constraint(equalTo: tabStack.trailingAnchor, constant: 6),
                newTabButton.centerYAnchor.constraint(equalTo: tabBar.centerYAnchor),
                newTabButton.widthAnchor.constraint(equalToConstant: 26),
                newTabButton.heightAnchor.constraint(equalToConstant: 24),
            ]
        }

        // Page region: the bitmap fills it; DevTools occupies its bottom.
        cs += [
            bitmapView.topAnchor.constraint(equalTo: pageTop),
            bitmapView.leadingAnchor.constraint(equalTo: pageLeading),
            bitmapView.trailingAnchor.constraint(equalTo: pageTrailing),
            devTools.leadingAnchor.constraint(equalTo: pageLeading),
            devTools.trailingAnchor.constraint(equalTo: pageTrailing),
            devTools.bottomAnchor.constraint(equalTo: pageBottom),
        ]
        NSLayoutConstraint.activate(cs)
        tabLayoutConstraints = cs

        // Bitmap-bottom toggle rebuilt against this layout's page bottom (DevTools hidden) / panel top.
        bitmapBottomToContent = bitmapView.bottomAnchor.constraint(equalTo: pageBottom)
        bitmapBottomToDevTools = bitmapView.bottomAnchor.constraint(equalTo: devTools.topAnchor)
        (devToolsVisible ? bitmapBottomToDevTools : bitmapBottomToContent)?.isActive = true

        content.layoutSubtreeIfNeeded()
        updateViewport()
        refresh()
    }

    @objc private func toggleDevTools() {
        devToolsVisible.toggle()
        devTools.isHidden = !devToolsVisible
        // Closing DevTools clears any Elements-inspector highlight so it doesn't linger on the page
        // (done before the refresh below so the repaint drops the overlay).
        if !devToolsVisible, let engine = activeTab?.engine {
            browser_engine_set_inspect_node(engine, -1)
        }
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

    /// Set the address-bar leading icon to the active tab's favicon, falling back to the globe
    /// symbol when there's no icon (e.g. the New Tab page or a page without one).
    func updateAddressBarIcon() {
        if let favicon = activeTab?.favicon {
            lockSymbol.contentTintColor = nil // don't tint a real (non-template) icon
            lockSymbol.image = favicon
        } else {
            lockSymbol.contentTintColor = NSColor.secondaryLabelColor
            lockSymbol.image = NSImage(systemSymbolName: "globe", accessibilityDescription: nil)
        }
    }

    /// A progressive frame painted by the engine WHILE a page streams in (pushed from the load
    /// thread via the C callback). Only show it if `tab` is still the visible tab.
    func displayProgressFrame(forTab tab: Tab, data: Data, width: Int, height: Int, stride: Int) {
        guard tab === activeTab else { return }
        setBitmapImage(data: data, width: width, height: height, stride: stride)
    }

    // MARK: Navigation

    @objc private func navigate(_ sender: Any?) {
        // URL fixup (schemeless → https, `about:`/`data:` passthrough), the https→http fallback for
        // http-only sites, and HSTS all live in the Rust engine so every shell behaves identically.
        // We pass the raw text through and reconcile the address bar with the engine's committed URL
        // once the load finishes (see `load`).
        let raw = urlField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty else { return }
        load(urlString: raw, recordHistory: true)
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
            let url = urlField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
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

        // Show the requested text optimistically; the engine may resolve it to a different committed
        // URL (fixup, HSTS upgrade, redirect, http fallback), which we reconcile after the load.
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
            // The engine's committed URL after fixup/HSTS/redirect/http-fallback — what the address
            // bar and history should actually reflect (falls back to the requested text if absent).
            let committed = browser_engine_current_url(engine).map { String(cString: $0) } ?? urlString
            // Snapshot the favicon on the engine queue (its pixels are valid only until the next load).
            let favicon = faviconImage(from: engine)
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
                // Reconcile to the engine's committed URL, then record it in history (so a defaulted
                // https that fell back to http, or an HSTS upgrade, lands correctly in both).
                tab.urlString = committed
                if shouldRecord { tab.recordHistory(committed) }
                if self.activeTab === tab { self.urlField.stringValue = committed }
                tab.favicon = favicon
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
                    self.updateAddressBarIcon()
                    self.refresh()
                    self.updateNavButtons()
                }
            }
        }
    }
}

/// Build an `NSImage` from the engine's current favicon bitmap (straight-alpha RGBA8), or `nil` if
/// the page has no icon. Copies the pixels, so it's safe to call on the tab's engine queue (where
/// the borrowed pointer is valid until the next load).
private func faviconImage(from engine: OpaquePointer) -> NSImage? {
    let fb = browser_engine_favicon(engine)
    guard let pixels = fb.pixels, fb.width > 0, fb.height > 0 else { return nil }
    let w = Int(fb.width), h = Int(fb.height), stride = Int(fb.stride)
    let data = Data(bytes: pixels, count: stride * h) as CFData
    guard let provider = CGDataProvider(data: data) else { return nil }
    // Engine favicons are straight (non-premultiplied) alpha RGBA8.
    let bitmapInfo = CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue)
    guard let cg = CGImage(
        width: w, height: h,
        bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: stride,
        space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: bitmapInfo,
        provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
    ) else { return nil }
    return NSImage(cgImage: cg, size: NSSize(width: 16, height: 16))
}

