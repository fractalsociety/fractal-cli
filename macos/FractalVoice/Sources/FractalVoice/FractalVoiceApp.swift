import AppKit
import Combine
import Darwin
import SwiftUI

@main
@MainActor
final class FractalVoiceApp: NSObject, NSApplicationDelegate, NSMenuDelegate, NSWindowDelegate {
    private let coordinator = BuildCoordinator()
    private var statusItem: NSStatusItem!
    private var hotKey: GlobalHotKey?
    private var onboardingWindow: NSWindow?
    private var observations = Set<AnyCancellable>()

    static func main() {
        if ProcessInfo.processInfo.environment["FRACTAL_VOICE_SELF_TEST"] == "1" {
            do {
                guard let modelURL = BuildCoordinator.offlineModelURL(),
                      BuildCoordinator.fractalExecutable() != nil else {
                    throw OfflineSelfTestError.missingAssets
                }
                let recorder = try NativeVoiceRecorder(modelURL: modelURL)
                recorder.close()
                print("Fractal Voice offline runtime: ready")
                exit(0)
            } catch {
                FileHandle.standardError.write(
                    Data("Fractal Voice offline runtime: \(error)\n".utf8)
                )
                exit(1)
            }
        }
        let application = NSApplication.shared
        let delegate = FractalVoiceApp()
        application.delegate = delegate
        application.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        configureStatusItem()
        hotKey = GlobalHotKey { [weak self] in
            self?.coordinator.toggleRecording()
        }
        coordinator.$state
            .sink { [weak self] state in self?.updateStatusIcon(state) }
            .store(in: &observations)

        if !UserDefaults.standard.bool(forKey: "completedOnboarding") {
            showOnboarding()
        }
    }

    private func configureStatusItem() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        statusItem.button?.image = NSImage(
            systemSymbolName: "waveform.circle",
            accessibilityDescription: "Fractal Voice"
        )
        statusItem.button?.toolTip = "Fractal Voice — \(GlobalHotKey.displayName)"
        let menu = NSMenu()
        menu.delegate = self
        statusItem.menu = menu
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()

        let status = NSMenuItem(title: coordinator.state.label, action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)

        let detail = NSMenuItem(title: coordinator.latestActivity, action: nil, keyEquivalent: "")
        detail.isEnabled = false
        menu.addItem(detail)
        menu.addItem(.separator())

        let toggleTitle = coordinator.state == .recording ? "Stop Recording & Build" : "Start Recording"
        let toggle = NSMenuItem(
            title: toggleTitle,
            action: #selector(toggleRecording),
            keyEquivalent: " "
        )
        toggle.keyEquivalentModifierMask = [.control, .option]
        toggle.target = self
        toggle.isEnabled = ![.building, .preparing].contains(coordinator.state)
        menu.addItem(toggle)

        menu.addItem(item("Show Welcome", #selector(showOnboarding)))
        menu.addItem(item("Open Projects", #selector(openProjects)))
        menu.addItem(item("Open Activity Log", #selector(openLog)))
        menu.addItem(.separator())
        menu.addItem(item("Stop All Fractal Builds", #selector(stopBuilds)))
        menu.addItem(.separator())
        menu.addItem(item("Quit Fractal Voice", #selector(quit), key: "q"))
    }

    private func item(_ title: String, _ action: Selector, key: String = "") -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.target = self
        return item
    }

    @objc private func toggleRecording() {
        coordinator.toggleRecording()
    }

    @objc func showOnboarding() {
        if let onboardingWindow {
            onboardingWindow.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        NSApp.setActivationPolicy(.regular)
        let view = OnboardingView(coordinator: coordinator) { [weak self] in
            UserDefaults.standard.set(true, forKey: "completedOnboarding")
            self?.onboardingWindow?.close()
        }
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 680, height: 500),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Welcome to Fractal Voice"
        window.center()
        window.contentView = NSHostingView(rootView: view)
        window.isReleasedWhenClosed = false
        window.delegate = self
        onboardingWindow = window
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func windowWillClose(_ notification: Notification) {
        onboardingWindow = nil
        NSApp.setActivationPolicy(.accessory)
    }

    @objc private func openProjects() {
        coordinator.openProjects()
    }

    @objc private func openLog() {
        coordinator.openLog()
    }

    @objc private func stopBuilds() {
        coordinator.stopAllBuilds()
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    private func updateStatusIcon(_ state: VoiceState) {
        let symbol: String
        switch state {
        case .idle: symbol = "waveform.circle"
        case .preparing: symbol = "sparkles"
        case .recording: symbol = "record.circle.fill"
        case .building: symbol = "hammer.circle.fill"
        case .failed: symbol = "exclamationmark.circle.fill"
        }
        statusItem.button?.image = NSImage(
            systemSymbolName: symbol,
            accessibilityDescription: state.label
        )
    }
}

private enum OfflineSelfTestError: Error {
    case missingAssets
}
