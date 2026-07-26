import AppKit
import Foundation
import UserNotifications

enum VoiceState: Equatable {
    case idle
    case recording
    case building
    case failed(String)

    var label: String {
        switch self {
        case .idle: return "Ready"
        case .recording: return "Listening…"
        case .building: return "Building…"
        case .failed(let message): return message
        }
    }
}

@MainActor
final class BuildCoordinator: ObservableObject {
    @Published private(set) var state: VoiceState = .idle
    @Published private(set) var latestActivity = "Press ⌃⌥Space to speak"
    @Published private(set) var voiceReady = false

    private var process: Process?
    private var input: FileHandle?
    private var outputBuffer = ""
    private var hud: RecordingHUD?

    let projectsURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("fractal-projects", isDirectory: true)
    let logURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/FractalVoice.log")

    init() {
        checkVoiceReadiness()
        try? FileManager.default.createDirectory(
            at: projectsURL,
            withIntermediateDirectories: true
        )
    }

    func toggleRecording() {
        switch state {
        case .idle, .failed:
            startRecording()
        case .recording:
            stopRecordingAndBuild()
        case .building:
            NSSound.beep()
            latestActivity = "A build is already running"
        }
    }

    func startRecording() {
        guard process == nil else { return }
        guard let executable = Self.fractalExecutable() else {
            state = .failed("Fractal CLI not found")
            latestActivity = "Reinstall Fractal Voice or install the Fractal CLI"
            return
        }
        guard voiceReady else {
            state = .failed("Voice model needs setup")
            latestActivity = "Open Welcome and install the local voice model"
            return
        }

        let task = Process()
        let stdin = Pipe()
        let combinedOutput = Pipe()
        task.executableURL = executable
        task.arguments = ["voice", "--app-control", "--managed-project"]
        task.environment = Self.processEnvironment()
        task.standardInput = stdin
        task.standardOutput = combinedOutput
        task.standardError = combinedOutput

        outputBuffer = ""
        combinedOutput.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor in
                self?.consume(text)
            }
        }
        task.terminationHandler = { [weak self] finished in
            combinedOutput.fileHandleForReading.readabilityHandler = nil
            Task { @MainActor in
                self?.finished(exitCode: finished.terminationStatus)
            }
        }

        do {
            try task.run()
            process = task
            input = stdin.fileHandleForWriting
            state = .recording
            latestActivity = "Listening locally — press ⌃⌥Space again to build"
            hud = RecordingHUD()
            hud?.show()
            NSSound(named: "Tink")?.play()
        } catch {
            state = .failed("Could not start voice capture")
            latestActivity = error.localizedDescription
        }
    }

    func stopRecordingAndBuild() {
        guard state == .recording, let input else { return }
        input.write(Data("\n".utf8))
        try? input.close()
        self.input = nil
        state = .building
        latestActivity = "Transcribing locally, then starting your build…"
        hud?.showBuilding()
        NSSound(named: "Pop")?.play()
    }

    func stopAllBuilds() {
        guard let executable = Self.fractalExecutable() else { return }
        let stop = Process()
        stop.executableURL = executable
        stop.arguments = ["stop", "--all"]
        stop.environment = Self.processEnvironment()
        try? stop.run()
        latestActivity = "Stop requested for all Fractal builds"
    }

    func installVoiceModel() {
        guard process == nil, let executable = Self.fractalExecutable() else {
            state = .failed("Fractal CLI not found")
            return
        }
        let task = Process()
        let output = Pipe()
        task.executableURL = executable
        task.arguments = ["voice", "setup"]
        task.environment = Self.processEnvironment()
        task.standardOutput = output
        task.standardError = output
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor in
                self?.consume(text)
            }
        }
        task.terminationHandler = { [weak self] finished in
            output.fileHandleForReading.readabilityHandler = nil
            Task { @MainActor in
                self?.process = nil
                self?.voiceReady = finished.terminationStatus == 0
                self?.state = finished.terminationStatus == 0
                    ? .idle
                    : .failed("Voice setup failed")
                self?.latestActivity = finished.terminationStatus == 0
                    ? "Voice is ready — press ⌃⌥Space"
                    : "See the Fractal Voice log for setup details"
            }
        }
        do {
            try task.run()
            process = task
            state = .building
            latestActivity = "Installing the private on-device voice model…"
        } catch {
            state = .failed("Could not start voice setup")
            latestActivity = error.localizedDescription
        }
    }

    func checkVoiceReadiness() {
        guard let executable = Self.fractalExecutable() else {
            voiceReady = false
            return
        }
        let task = Process()
        let output = Pipe()
        task.executableURL = executable
        task.arguments = ["voice", "engines"]
        task.environment = Self.processEnvironment()
        task.standardOutput = output
        task.standardError = output
        do {
            try task.run()
            task.waitUntilExit()
            let data = output.fileHandleForReading.readDataToEndOfFile()
            let text = String(data: data, encoding: .utf8) ?? ""
            voiceReady = task.terminationStatus == 0 && text.contains("moonshine") && text.contains("ready")
        } catch {
            voiceReady = false
        }
    }

    func openProjects() {
        try? FileManager.default.createDirectory(
            at: projectsURL,
            withIntermediateDirectories: true
        )
        NSWorkspace.shared.open(projectsURL)
    }

    func openLog() {
        ensureLogParent()
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: Data())
        }
        NSWorkspace.shared.open(logURL)
    }

    private func consume(_ text: String) {
        appendLog(text)
        outputBuffer += text
        if outputBuffer.count > 32_000 {
            outputBuffer.removeFirst(outputBuffer.count - 32_000)
        }
        if text.contains("Created managed voice project:") {
            latestActivity = "Project created — lead agent is planning…"
        } else if text.contains("opening the project now") || text.contains("Planning graph:") {
            latestActivity = "Execution graph is live — planning your build…"
        } else if text.contains("executing with") || text.contains("→ executing in") {
            latestActivity = "Workers are building from the execution graph…"
        } else if text.contains("Interpreted instruction:") {
            latestActivity = "Instruction understood — creating the project…"
        }
    }

    private func finished(exitCode: Int32) {
        process = nil
        input = nil
        hud?.close()
        hud = nil
        if exitCode == 0 {
            state = .idle
            latestActivity = "Build finished — press ⌃⌥Space for another project"
            notify(title: "Fractal build finished", body: "Your project run completed.")
        } else {
            let detail = outputBuffer
                .split(separator: "\n")
                .suffix(3)
                .joined(separator: " ")
            state = .failed("Build stopped")
            latestActivity = detail.isEmpty ? "Open the log for details" : detail
            notify(title: "Fractal needs attention", body: latestActivity)
        }
    }

    private func notify(title: String, body: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { _, _ in }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        center.add(UNNotificationRequest(
            identifier: UUID().uuidString,
            content: content,
            trigger: nil
        ))
    }

    private func appendLog(_ text: String) {
        ensureLogParent()
        let data = Data(text.utf8)
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: data)
            return
        }
        guard let handle = try? FileHandle(forWritingTo: logURL) else { return }
        defer { try? handle.close() }
        do {
            try handle.seekToEnd()
            try handle.write(contentsOf: data)
        } catch {
            return
        }
    }

    private func ensureLogParent() {
        try? FileManager.default.createDirectory(
            at: logURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
    }

    nonisolated static func fractalExecutable() -> URL? {
        let fileManager = FileManager.default
        let bundled = Bundle.main.resourceURL?.appendingPathComponent("fractal")
        let home = fileManager.homeDirectoryForCurrentUser
        let candidates = [
            bundled,
            home.appendingPathComponent(".cargo/bin/fractal"),
            URL(fileURLWithPath: "/opt/homebrew/bin/fractal"),
            URL(fileURLWithPath: "/usr/local/bin/fractal")
        ].compactMap { $0 }
        return candidates.first { fileManager.isExecutableFile(atPath: $0.path) }
    }

    nonisolated static func processEnvironment() -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let additions = [
            "\(home)/.cargo/bin",
            "\(home)/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin"
        ]
        environment["PATH"] = additions.joined(separator: ":")
        environment["HOME"] = home
        return environment
    }
}
