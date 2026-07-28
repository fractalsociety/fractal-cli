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
    private var externalHandoffTimer: Timer?
    private var setupComplete = false
    private var selectedVoiceMode: VoiceInputMode?
    private var lastGlobalHotKeyActivation: TimeInterval = 0

    static func main() {
        if ProcessInfo.processInfo.environment["FRACTAL_VOICE_SELF_TEST"] == "1" {
            do {
                guard BuildCoordinator.graniteAssets() != nil,
                      BuildCoordinator.graniteExecutable() != nil,
                      BuildCoordinator.graniteServerExecutable() != nil,
                      (try? KokoroSpeaker.assets()) != nil,
                      BuildCoordinator.fractalExecutable() != nil else {
                    throw OfflineSelfTestError.missingAssets
                }
                let shortcut = try GlobalHotKey {}
                try KokoroSpeaker.synthesisSelfTest()
                withExtendedLifetime(shortcut) {}
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
        setupComplete = UserDefaults.standard.bool(forKey: "completedOnboarding")
        selectedVoiceMode = VoiceInputMode.selected()
        if setupComplete, selectedVoiceMode == nil, coordinator.voiceReady {
            VoiceInputMode.save(.builtIn)
            selectedVoiceMode = .builtIn
        }
        if setupComplete,
           let selectedVoiceMode,
           selectedVoiceMode.isReady(localModelsReady: coordinator.voiceReady) {
            activate(selectedVoiceMode)
        } else {
            setupComplete = false
            coordinator.reportSetupRequired()
        }
        NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.didWakeNotification)
            .sink { [weak self] _ in
                guard
                    self?.setupComplete == true,
                    self?.selectedVoiceMode == .builtIn
                else { return }
                self?.installGlobalHotKey()
            }
            .store(in: &observations)
        coordinator.$state
            .sink { [weak self] state in self?.updateStatusIcon(state) }
            .store(in: &observations)
        startExternalHandoffMonitoring()

        if !setupComplete {
            showOnboarding()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        externalHandoffTimer?.invalidate()
        coordinator.shutdown()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard coordinator.hasActiveBuild else {
            return .terminateNow
        }

        let alert = NSAlert()
        alert.messageText = "A Fractal build is still running"
        alert.informativeText =
            "Quitting now could interrupt the active agents. Keep Fractal Voice open, "
            + "or pause the build safely and preserve its completed tasks before quitting."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Keep Building")
        alert.addButton(withTitle: "Pause Build and Quit")

        guard alert.runModal() == .alertSecondButtonReturn else {
            return .terminateCancel
        }
        coordinator.pauseBuildForApplicationTermination {
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }

    func application(_ sender: NSApplication, openFiles filenames: [String]) {
        guard filenames.count == 1 else {
            sender.reply(toOpenOrPrint: .failure)
            return
        }
        let url = URL(fileURLWithPath: filenames[0])
        let handled = url.pathExtension == "fractalxshare"
            ? handleExternalXShare(at: url, reportFailure: true)
            : handleExternalBuild(at: url, reportFailure: true)
        sender.reply(toOpenOrPrint: handled ? .success : .failure)
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        guard urls.count == 1 else { return }
        do {
            if urls[0].host?.lowercased() == "visibility" {
                let handoff = try WebsiteVisibilityHandoff(url: urls[0])
                guard setupComplete else {
                    showOnboarding()
                    throw WebsiteTaskHandoffError.setupRequired
                }
                let alert = NSAlert()
                alert.messageText = "Make \(handoff.project) \(handoff.target)?"
                alert.informativeText =
                    "Fractal Voice will use your authenticated GitHub CLI to make the "
                    + "repository and Fractal Society graph \(handoff.target)."
                alert.alertStyle = handoff.target == "public" ? .warning : .informational
                alert.addButton(withTitle: "Yes, make \(handoff.target)")
                alert.addButton(withTitle: "Cancel")
                guard alert.runModal() == .alertFirstButtonReturn else { return }
                coordinator.applyWebsiteVisibility(
                    project: handoff.project,
                    target: handoff.target
                )
                return
            }
            let handoff = try WebsiteTaskHandoff(url: urls[0])
            guard setupComplete else {
                showOnboarding()
                throw WebsiteTaskHandoffError.setupRequired
            }
            try coordinator.startWebsiteTask(
                token: handoff.token,
                server: handoff.server,
                action: handoff.action
            )
        } catch {
            coordinator.reportExternalBuildFailure(error.localizedDescription)
        }
    }

    private func startExternalHandoffMonitoring() {
        externalHandoffTimer?.invalidate()
        externalHandoffTimer = Timer.scheduledTimer(
            withTimeInterval: 0.75,
            repeats: true
        ) { [weak self] _ in
            Task { @MainActor in
                self?.consumeNextQueuedVisibility()
                self?.consumeNextQueuedXShare()
                self?.consumeNextQueuedExternalBuild()
            }
        }
        consumeNextQueuedVisibility()
        consumeNextQueuedXShare()
        consumeNextQueuedExternalBuild()
    }

    private func consumeNextQueuedVisibility() {
        guard setupComplete, let url = ExternalVisibilityHandoff.pendingURLs().first else {
            return
        }
        do {
            let resultURL = url.deletingPathExtension().appendingPathExtension("result")
            let request = try ExternalVisibilityHandoff.consume(url)
            coordinator.applyExternalVisibility(request, resultURL: resultURL)
        } catch {
            coordinator.reportExternalBuildFailure(error.localizedDescription)
        }
    }

    private func consumeNextQueuedExternalBuild() {
        guard setupComplete, coordinator.canAcceptExternalBuild else { return }
        guard let url = ExternalBuildHandoff.pendingURLs().first else { return }
        _ = handleExternalBuild(at: url, reportFailure: true)
    }

    private func consumeNextQueuedXShare() {
        guard setupComplete, let url = ExternalXShareHandoff.pendingURLs().first else {
            return
        }
        _ = handleExternalXShare(at: url, reportFailure: true)
    }

    private func handleExternalXShare(at url: URL, reportFailure: Bool) -> Bool {
        guard setupComplete else {
            showOnboarding()
            if reportFailure {
                coordinator.reportExternalBuildFailure(
                    ExternalBuildLaunchError.setupRequired.localizedDescription
                )
            }
            return false
        }
        let resultURL = url.deletingPathExtension().appendingPathExtension("result")
        do {
            let request = try ExternalXShareHandoff.consume(url)
            Task { @MainActor [weak self] in
                try? await Task.sleep(nanoseconds: 250_000_000)
                guard let self else { return }
                let opened = self.openXComposer(request.intentURL)
                let message = opened
                    ? "Opened the approved X composer. Review the post and choose Post."
                    : ExternalXShareError.invalidRequest.localizedDescription
                self.writeXShareResult(
                    to: resultURL,
                    success: opened,
                    message: message
                )
                if opened {
                    self.coordinator.reportExternalShareOpened()
                } else if reportFailure {
                    self.coordinator.reportExternalBuildFailure(message)
                }
            }
            return true
        } catch {
            writeXShareResult(
                to: resultURL,
                success: false,
                message: error.localizedDescription
            )
            if reportFailure {
                coordinator.reportExternalBuildFailure(error.localizedDescription)
            }
            return false
        }
    }

    private func openXComposer(_ url: URL) -> Bool {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        task.arguments = [url.absoluteString]
        task.standardOutput = FileHandle.nullDevice
        task.standardError = FileHandle.nullDevice
        do {
            try task.run()
            task.waitUntilExit()
            return task.terminationStatus == 0
        } catch {
            return false
        }
    }

    private func writeXShareResult(
        to url: URL,
        success: Bool,
        message: String
    ) {
        guard let data = try? JSONSerialization.data(withJSONObject: [
            "success": success,
            "message": message,
        ]) else {
            return
        }
        try? FileManager.default.removeItem(at: url)
        _ = FileManager.default.createFile(
            atPath: url.path,
            contents: data,
            attributes: [.posixPermissions: 0o600]
        )
    }

    private func handleExternalBuild(at url: URL, reportFailure: Bool) -> Bool {
        guard setupComplete else {
            showOnboarding()
            if reportFailure {
                coordinator.reportExternalBuildFailure(
                    ExternalBuildLaunchError.setupRequired.localizedDescription
                )
            }
            return false
        }
        guard coordinator.canAcceptExternalBuild else {
            return false
        }
        do {
            let external = try ExternalBuildHandoff.consume(url)
            try coordinator.startExternalBuild(external)
            return true
        } catch {
            if reportFailure {
                coordinator.reportExternalBuildFailure(error.localizedDescription)
            }
            return false
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
        let shortcut = NSMenuItem(
            title: coordinator.shortcutStatus,
            action: nil,
            keyEquivalent: ""
        )
        shortcut.isEnabled = false
        menu.addItem(shortcut)
        menu.addItem(.separator())

        if selectedVoiceMode == .builtIn {
            let toggleTitle = coordinator.state == .recording
                ? "Stop Recording & Build"
                : "Start Recording"
            let toggle = NSMenuItem(
                title: toggleTitle,
                action: #selector(toggleRecording),
                keyEquivalent: ""
            )
            toggle.target = self
            toggle.isEnabled = setupComplete
                && ![.building, .preparing].contains(coordinator.state)
            menu.addItem(toggle)
            if setupComplete && !coordinator.shortcutReady {
                menu.addItem(item("Retry ⌥Space Shortcut", #selector(retryShortcut)))
            }
        } else if let selectedVoiceMode {
            let voiceSource = NSMenuItem(
                title: "Voice input: \(selectedVoiceMode.title)",
                action: nil,
                keyEquivalent: ""
            )
            voiceSource.isEnabled = false
            menu.addItem(voiceSource)
        }

        menu.addItem(item("Show Welcome", #selector(showOnboarding)))
        if coordinator.microphoneDenied {
            menu.addItem(item("Open Microphone Settings", #selector(openMicrophoneSettings)))
        }
        menu.addItem(item("Open Projects", #selector(openProjects)))
        menu.addItem(item("Change Project Location…", #selector(showProjectLocation)))
        menu.addItem(item("Open Activity Log", #selector(openLog)))
        menu.addItem(item("Support", #selector(openSupport)))
        menu.addItem(item("Privacy Policy", #selector(openPrivacyPolicy)))
        menu.addItem(.separator())
        if coordinator.state == .building {
            menu.addItem(item("Stop Current Build", #selector(stopCurrentBuild)))
            menu.addItem(item("Restart Voice Command", #selector(restartVoiceCommand)))
        }
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

    @objc private func retryShortcut() {
        installGlobalHotKey()
    }

    private func installGlobalHotKey() {
        hotKey = nil
        do {
            hotKey = try GlobalHotKey { [weak self] in
                self?.handleGlobalHotKey()
            }
            coordinator.reportShortcutReady()
        } catch {
            coordinator.reportShortcutFailure(
                error.localizedDescription
                    + " Quit the conflicting app, then choose Retry Shortcut from the Fractal menu."
            )
        }
    }

    private func handleGlobalHotKey() {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastGlobalHotKeyActivation >= 0.35 else { return }
        lastGlobalHotKeyActivation = now
        coordinator.toggleRecording()
    }

    @objc func showOnboarding() {
        presentOnboarding(initialPage: 0)
    }

    @objc private func showProjectLocation() {
        if onboardingWindow != nil {
            onboardingWindow?.close()
        }
        presentOnboarding(initialPage: AppRuntime.isAppStoreEdition ? 5 : 4)
    }

    private func presentOnboarding(initialPage: Int) {
        if let onboardingWindow {
            onboardingWindow.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        NSApp.setActivationPolicy(.regular)
        let view = OnboardingView(
            coordinator: coordinator,
            initialPage: initialPage
        ) { [weak self] in
            UserDefaults.standard.set(true, forKey: "completedOnboarding")
            self?.setupComplete = true
            self?.selectedVoiceMode = VoiceInputMode.selected()
            if let selectedVoiceMode = self?.selectedVoiceMode {
                self?.activate(selectedVoiceMode)
            }
            self?.onboardingWindow?.close()
        }
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 600),
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

    @objc private func openMicrophoneSettings() {
        coordinator.openMicrophoneSettings()
    }

    @objc private func openLog() {
        coordinator.openLog()
    }

    @objc private func openSupport() {
        NSWorkspace.shared.open(URL(string: "https://fractalsociety.com/support")!)
    }

    @objc private func openPrivacyPolicy() {
        NSWorkspace.shared.open(URL(string: "https://fractalsociety.com/privacy")!)
    }

    @objc private func stopBuilds() {
        coordinator.stopAllBuilds()
    }

    @objc private func stopCurrentBuild() {
        coordinator.stopCurrentBuild()
    }

    @objc private func restartVoiceCommand() {
        coordinator.restartVoiceCommand()
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

    private func activate(_ mode: VoiceInputMode) {
        selectedVoiceMode = mode
        switch mode {
        case .builtIn:
            coordinator.activateBuiltInVoice()
            installGlobalHotKey()
            coordinator.requestMicrophonePermission()
        case .chatGPTDesktop, .superwhisper:
            hotKey = nil
            coordinator.activateExternalVoice(mode)
        }
    }
}

private struct WebsiteTaskHandoff {
    let token: String
    let server: URL
    let action: String

    init(url: URL) throws {
        guard
            url.scheme?.lowercased() == "fractalvoice",
            ["work", "resume"].contains(url.host?.lowercased() ?? ""),
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let token = components.queryItems?.first(where: { $0.name == "token" })?.value,
            token.hasPrefix("fth_"),
            token.count <= 512,
            let serverValue = components.queryItems?.first(where: { $0.name == "server" })?.value,
            let server = URL(string: serverValue),
            server.scheme == "https",
            server.host?.lowercased() == "fractalsociety.com"
        else {
            throw WebsiteTaskHandoffError.invalid
        }
        self.token = token
        self.server = server
        self.action = url.host!.lowercased()
    }
}

struct WebsiteVisibilityHandoff {
    let project: String
    let target: String

    init(url: URL) throws {
        guard
            url.scheme?.lowercased() == "fractalvoice",
            url.host?.lowercased() == "visibility",
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let project = components.queryItems?.first(where: { $0.name == "project" })?.value?
                .trimmingCharacters(in: .whitespacesAndNewlines),
            !project.isEmpty,
            project.count <= 100,
            !project.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
            let target = components.queryItems?.first(where: { $0.name == "target" })?.value,
            ["public", "private"].contains(target),
            let server = components.queryItems?.first(where: { $0.name == "server" })?.value,
            server == "https://fractalsociety.com"
        else {
            throw WebsiteTaskHandoffError.invalid
        }
        self.project = project
        self.target = target
    }
}

private enum WebsiteTaskHandoffError: LocalizedError {
    case invalid
    case setupRequired

    var errorDescription: String? {
        switch self {
        case .invalid:
            return "The Fractal Society task handoff is invalid or came from an untrusted site."
        case .setupRequired:
            return "Complete Fractal Voice setup before opening a project task."
        }
    }
}

private enum ExternalBuildLaunchError: LocalizedError {
    case setupRequired

    var errorDescription: String? {
        "Complete Fractal Voice setup before accepting builds from another desktop app."
    }
}

private enum OfflineSelfTestError: Error {
    case missingAssets
}
