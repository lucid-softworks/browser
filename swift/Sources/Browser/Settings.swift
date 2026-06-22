import AppKit
import Foundation

// MARK: - Config

/// User settings, persisted as JSON in the per-user app data dir
/// (`~/Library/Application Support/dev.imlunahey.browser/config.json`). Loaded once at launch and
/// rewritten whenever a setting changes via the Settings window.
final class Config {
    static let shared = Config()

    private let fileURL: URL
    var homepage: String { didSet { save() } }

    private init() {
        let fm = FileManager.default
        let base = (fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support"))
            .appendingPathComponent("dev.imlunahey.browser", isDirectory: true)
        try? fm.createDirectory(at: base, withIntermediateDirectories: true)
        fileURL = base.appendingPathComponent("config.json")

        var hp = Config.defaultHomepage
        if let data = try? Data(contentsOf: fileURL),
           let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let h = obj["homepage"] as? String, !h.isEmpty {
            hp = h
        }
        homepage = hp
    }

    /// Default homepage and new-tab page: `about:blank`, the empty initial document (matching real
    /// browsers' default). Override it via the Settings window.
    static var defaultHomepage: String { "about:blank" }

    private func save() {
        let obj: [String: Any] = ["homepage": homepage]
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
    private let currentURLProvider: () -> String?

    init(currentURLProvider: @escaping () -> String?) {
        self.currentURLProvider = currentURLProvider
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 150),
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

            save.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -20),
            save.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -20),
            useCurrent.trailingAnchor.constraint(equalTo: save.leadingAnchor, constant: -10),
            useCurrent.centerYAnchor.constraint(equalTo: save.centerYAnchor),
        ])
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

