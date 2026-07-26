import AppKit
import Foundation
import UserNotifications

enum VoiceState: Equatable {
    case idle
    case preparing
    case recording
    case building
    case failed(String)

    var label: String {
        switch self {
        case .idle: return "Ready"
        case .preparing: return "Starting voice engine…"
        case .recording: return "Listening…"
        case .building: return "Building…"
        case .failed(let message): return message
        }
    }
}

@MainActor
final class BuildCoordinator: ObservableObject {
    @Published private(set) var state: VoiceState = .idle
    @Published private(set) var latestActivity = "Checking bundled offline assets…"
    @Published private(set) var voiceReady = false

    private var process: Process?
    private var recorder: NativeVoiceRecorder?
    private var outputBuffer = ""
    private var hud: RecordingHUD?

    let projectsURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("fractal-projects", isDirectory: true)
    let logURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/FractalVoice.log")

    init() {
        try? FileManager.default.createDirectory(
            at: projectsURL,
            withIntermediateDirectories: true
        )
        voiceReady = Self.offlineModelURL() != nil && Self.fractalExecutable() != nil
        latestActivity = voiceReady
            ? "Press ⌃⌥Space to speak"
            : "Offline assets are missing — reinstall Fractal Voice"
    }

    func toggleRecording() {
        switch state {
        case .idle, .failed:
            startRecording()
        case .preparing:
            NSSound.beep()
            latestActivity = "The offline voice engine is still starting"
        case .recording:
            stopRecordingAndBuild()
        case .building:
            NSSound.beep()
            latestActivity = "A build is already running"
        }
    }

    func startRecording() {
        guard process == nil else { return }
        guard voiceReady, let modelURL = Self.offlineModelURL() else {
            state = .failed("Offline voice assets are not ready")
            latestActivity = "Reinstall the complete Fractal Voice application"
            return
        }
        if let recorder {
            beginRecording(with: recorder)
            return
        }

        state = .preparing
        latestActivity = "Loading Moonshine v2 Medium locally…"
        hud = RecordingHUD()
        hud?.showPreparing()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            do {
                let recorder = try NativeVoiceRecorder(modelURL: modelURL)
                self?.configureCallbacks(for: recorder)
                Task { @MainActor in
                    self?.recorder = recorder
                    self?.beginRecording(with: recorder)
                }
            } catch {
                Task { @MainActor in
                    self?.recordingFailed(error)
                }
            }
        }
    }

    private func beginRecording(with recorder: NativeVoiceRecorder) {
        do {
            try recorder.start()
            state = .recording
            latestActivity = "Listening locally — press ⌃⌥Space again to build"
            hud?.showListening()
            NSSound(named: "Tink")?.play()
        } catch {
            state = .failed("Could not start microphone capture")
            latestActivity = error.localizedDescription
        }
    }

    func stopRecordingAndBuild() {
        guard state == .recording, let recorder else { return }
        state = .building
        latestActivity = "Finishing the local transcript…"
        hud?.showBuilding()
        NSSound(named: "Pop")?.play()

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            do {
                let transcript = try recorder.stop()
                recorder.close()
                Task { @MainActor in
                    self?.recorder = nil
                    self?.startBuild(transcript: transcript)
                }
            } catch {
                Task { @MainActor in
                    self?.recordingFailed(error)
                }
            }
        }
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

    nonisolated private func configureCallbacks(for recorder: NativeVoiceRecorder) {
        recorder.onPartialTranscript = { [weak self] transcript in
            guard !transcript.isEmpty else { return }
            Task { @MainActor in
                self?.latestActivity = transcript
            }
        }
        recorder.onError = { [weak self] message in
            Task { @MainActor in
                self?.latestActivity = message
            }
        }
    }

    private func startBuild(transcript: String) {
        let transcript = transcript.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !transcript.isEmpty else {
            recordingFailed(VoiceAppError.noSpeech)
            return
        }
        guard let executable = Self.fractalExecutable() else {
            recordingFailed(VoiceAppError.cliMissing)
            return
        }

        let task = Process()
        let stdin = Pipe()
        let combinedOutput = Pipe()
        task.executableURL = executable
        task.arguments = [
            "ingest",
            "--source", "fractal-mac-app",
            "--format", "text",
            "--stdin",
            "--managed-project"
        ]
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
            stdin.fileHandleForWriting.write(Data(transcript.utf8))
            try stdin.fileHandleForWriting.close()
            latestActivity = "Instruction understood — creating the project…"
        } catch {
            recordingFailed(error)
        }
    }

    private func recordingFailed(_ error: Error) {
        process = nil
        recorder?.close()
        recorder = nil
        hud?.close()
        hud = nil
        state = .failed("Voice command stopped")
        latestActivity = error.localizedDescription
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

    nonisolated static func offlineModelURL() -> URL? {
        guard let model = Bundle.main.resourceURL?
            .appendingPathComponent("MoonshineModels/medium-streaming-en", isDirectory: true)
        else {
            return nil
        }
        let required = [
            "adapter.ort",
            "cross_kv.ort",
            "decoder_kv.ort",
            "decoder_kv_with_attention.ort",
            "encoder.ort",
            "frontend.ort",
            "streaming_config.json",
            "tokenizer.bin"
        ]
        return required.allSatisfy {
            FileManager.default.fileExists(atPath: model.appendingPathComponent($0).path)
        } ? model : nil
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

private enum VoiceAppError: LocalizedError {
    case noSpeech
    case cliMissing

    var errorDescription: String? {
        switch self {
        case .noSpeech: return "No speech was detected. Press the shortcut and try again."
        case .cliMissing: return "The bundled Fractal CLI is missing."
        }
    }
}
