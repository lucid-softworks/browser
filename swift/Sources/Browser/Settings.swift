import AppKit
import Foundation

// MARK: - Config

/// User settings, persisted as JSON in the per-user app data dir
/// (`~/Library/Application Support/dev.imlunahey.browser/config.json`). Loaded once at launch and
/// rewritten whenever a setting changes via the Settings window.
/// Where the tab strip is placed around the page area.
enum TabPosition: String, CaseIterable {
    case top, bottom, left, right

    var label: String {
        switch self {
        case .top: return "Top"
        case .bottom: return "Bottom"
        case .left: return "Left"
        case .right: return "Right"
        }
    }
}

final class Config {
    static let shared = Config()

    private let fileURL: URL
    var homepage: String { didSet { save() } }
    /// Tab strip position. Changing it re-lays-out the window live via `onTabPositionChange`.
    var tabPosition: TabPosition { didSet { save(); onTabPositionChange?(tabPosition) } }
    /// Set by the AppDelegate to re-apply the window layout when the position changes.
    var onTabPositionChange: ((TabPosition) -> Void)?

    private init() {
        let fm = FileManager.default
        let base = (fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support"))
            .appendingPathComponent("dev.imlunahey.browser", isDirectory: true)
        try? fm.createDirectory(at: base, withIntermediateDirectories: true)
        fileURL = base.appendingPathComponent("config.json")

        var hp = Config.defaultHomepage
        var tp = TabPosition.top
        if let data = try? Data(contentsOf: fileURL),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            if let h = obj["homepage"] as? String, !h.isEmpty { hp = h }
            if let t = obj["tabPosition"] as? String, let parsed = TabPosition(rawValue: t) { tp = parsed }
        }
        homepage = hp
        tabPosition = tp
    }

    /// Default homepage and new-tab page: `about:blank`, the empty initial document (matching real
    /// browsers' default). Override it via the Settings window.
    static var defaultHomepage: String { "about:blank" }

    private func save() {
        let obj: [String: Any] = ["homepage": homepage, "tabPosition": tabPosition.rawValue]
        if let data = try? JSONSerialization.data(withJSONObject: obj, options: [.prettyPrinted, .sortedKeys]) {
            try? data.write(to: fileURL)
        }
    }
}

// MARK: - Settings window

/// A small Settings window with a homepage field, persisted to `Config`. The homepage takes effect
/// on the next new tab/window (the default URL is read live from `Config`).
final class SettingsWindowController: NSWindowController, NSWindowDelegate {
    private let homepageField = NSTextField()
    private let tabPositionPopup = NSPopUpButton()
    private let currentURLProvider: () -> String?

    init(currentURLProvider: @escaping () -> String?) {
        self.currentURLProvider = currentURLProvider
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 210),
            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Settings"
        super.init(window: window)
        window.delegate = self
        window.center()
        buildContent()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    private func buildContent() {
        guard let content = window?.contentView else { return }

        let title = NSTextField(labelWithString: "Homepage")
        title.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        title.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(title)

        homepageField.stringValue = Config.shared.homepage
        homepageField.placeholderString = "https://example.com"
        homepageField.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(homepageField)

        let useCurrent = NSButton(title: "Use Current Page", target: self, action: #selector(useCurrentPage))
        useCurrent.bezelStyle = .rounded
        useCurrent.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(useCurrent)

        let tabTitle = NSTextField(labelWithString: "Tab Position")
        tabTitle.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        tabTitle.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(tabTitle)

        tabPositionPopup.translatesAutoresizingMaskIntoConstraints = false
        tabPositionPopup.addItems(withTitles: TabPosition.allCases.map { $0.label })
        if let idx = TabPosition.allCases.firstIndex(of: Config.shared.tabPosition) {
            tabPositionPopup.selectItem(at: idx)
        }
        tabPositionPopup.target = self
        tabPositionPopup.action = #selector(tabPositionChanged)
        content.addSubview(tabPositionPopup)

        let save = NSButton(title: "Save", target: self, action: #selector(saveAndClose))
        save.bezelStyle = .rounded
        save.keyEquivalent = "\r"
        save.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(save)

        NSLayoutConstraint.activate([
            title.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            title.topAnchor.constraint(equalTo: content.topAnchor, constant: 20),

            homepageField.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            homepageField.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -20),
            homepageField.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 8),

            tabTitle.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            tabTitle.topAnchor.constraint(equalTo: homepageField.bottomAnchor, constant: 18),
            tabPositionPopup.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            tabPositionPopup.topAnchor.constraint(equalTo: tabTitle.bottomAnchor, constant: 8),
            tabPositionPopup.widthAnchor.constraint(equalToConstant: 160),

            save.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -20),
            save.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -20),
            useCurrent.trailingAnchor.constraint(equalTo: save.leadingAnchor, constant: -10),
            useCurrent.centerYAnchor.constraint(equalTo: save.centerYAnchor),
        ])
    }

    @objc private func tabPositionChanged() {
        let idx = tabPositionPopup.indexOfSelectedItem
        guard idx >= 0, idx < TabPosition.allCases.count else { return }
        Config.shared.tabPosition = TabPosition.allCases[idx] // triggers live re-layout
    }

    @objc private func useCurrentPage() {
        if let url = currentURLProvider(), !url.isEmpty { homepageField.stringValue = url }
    }

    @objc private func saveAndClose() {
        let v = homepageField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        if !v.isEmpty { Config.shared.homepage = v }
        window?.close()
    }
}

