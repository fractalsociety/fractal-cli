import AppKit
import AVFoundation
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

private enum TranscriptionPurpose: Equatable {
    case request
    case amendment
    case requestConfirmation
    case projectName
    case nameConfirmation
}

private enum DialogueStage: Equatable {
    case none
    case confirmingRequest
    case awaitingRequestAnswer
    case awaitingProjectName
    case confirmingName
    case awaitingNameAnswer
}

@MainActor
final class BuildCoordinator: ObservableObject {
    @Published private(set) var state: VoiceState = .idle
    @Published private(set) var latestActivity = "Checking offline voice engine…"
    @Published private(set) var voiceReady = false
    @Published private(set) var shortcutReady = false
    @Published private(set) var shortcutStatus = "Registering ⌥Space…"
    @Published private(set) var microphoneDenied = false

    private var process: Process?
    private var transcriptionProcess: Process?
    private var graniteServerProcess: Process?
    private var graniteServerBaseURL: URL?
    private var transcriptionStartedAt: Date?
    private var transcriptionUsedWarmServer = false
    private var recorder: NativeVoiceRecorder?
    private var activeAudioURL: URL?
    private var outputBuffer = ""
    private var outputLineBuffer = ""
    private var hud: RecordingHUD?
    private var stopCommand: Process?
    private var bridgeBuildTask: Task<Void, Never>?
    private var stopRequested = false
    private var restartRequested = false
    private var activeWorkspace: URL?
    private let vocabularyEngine = VoiceVocabularyEngine(homeURL: AppRuntime.homeURL)
    private let speaker = KokoroSpeaker()
    private var transcriptionPurpose: TranscriptionPurpose = .request
    private var dialogueStage: DialogueStage = .none
    private var pendingRequest = ""
    private var pendingRequestWasTyped = false
    private var pendingProjectName = ""
    private var dialogueGeneration = 0
    private var transcriptRetryCount = 0
    private var recordingTimeout: Task<Void, Never>?
    private var terminationAfterPause: (() -> Void)?

    var projectsURL: URL { AppRuntime.projectsURL }
    let logURL = AppRuntime.logURL

    var hasActiveBuild: Bool {
        process?.isRunning == true || bridgeBuildTask != nil
    }

    var canAcceptExternalBuild: Bool {
        process == nil
            && transcriptionProcess == nil
            && recorder == nil
            && state != .preparing
            && state != .recording
            && state != .building
    }

    init() {
        try? AppRuntime.prepareProjectsDirectory()
        try? vocabularyEngine.installPersonalTemplateIfNeeded()
        refreshVoiceReadiness()
    }

    func refreshVoiceReadiness() {
        voiceReady = Self.graniteAssets() != nil
            && Self.graniteExecutable() != nil
            && (try? KokoroSpeaker.assets()) != nil
            && Self.fractalExecutable() != nil
        latestActivity = voiceReady
            ? "Press ⌥Space to speak"
            : "Voice models are downloading — open Welcome for progress"
    }

    func activateBuiltInVoice() {
        refreshVoiceReadiness()
        if voiceReady {
            startGraniteServer()
        }
    }

    func activateExternalVoice(_ mode: VoiceInputMode) {
        graniteServerProcess?.terminate()
        graniteServerProcess = nil
        graniteServerBaseURL = nil
        latestActivity = switch mode {
        case .chatGPTDesktop:
            "Ready for secure builds from ChatGPT Desktop"
        case .superwhisper:
            "Ready for commands from Superwhisper"
        case .builtIn:
            "Press ⌥Space to speak"
        }
    }

    private func startGraniteServer() {
        guard
            graniteServerProcess?.isRunning != true,
            let executable = Self.graniteServerExecutable(),
            let assets = Self.graniteAssets()
        else {
            return
        }
        let port = AppRuntime.graniteServerPort
        let server = Process()
        server.executableURL = executable
        server.arguments = [
            "--model", assets.model.path,
            "--mmproj", assets.projector.path,
            "--host", "127.0.0.1",
            "--port", String(port),
            "--ctx-size", "4096",
            "--parallel", "1",
            "--no-webui",
            "--log-disable"
        ]
        server.environment = Self.processEnvironment()
        server.standardInput = FileHandle.nullDevice
        server.standardOutput = FileHandle.nullDevice
        server.standardError = FileHandle.nullDevice
        server.terminationHandler = { [weak self, weak server] _ in
            Task { @MainActor in
                guard
                    let self,
                    let server,
                    self.graniteServerProcess === server
                else {
                    return
                }
                self.graniteServerProcess = nil
                self.graniteServerBaseURL = nil
            }
        }
        do {
            try server.run()
            graniteServerProcess = server
            graniteServerBaseURL = URL(string: "http://127.0.0.1:\(port)")
        } catch {
            graniteServerProcess = nil
            graniteServerBaseURL = nil
            appendLog("[voice] warm Granite server unavailable: \(error)\n")
        }
    }

    func shutdown() {
        recordingTimeout?.cancel()
        bridgeBuildTask?.cancel()
        bridgeBuildTask = nil
        graniteServerBaseURL = nil
        if graniteServerProcess?.isRunning == true {
            graniteServerProcess?.terminate()
        }
        graniteServerProcess = nil
    }

    func pauseBuildForApplicationTermination(completion: @escaping () -> Void) {
        guard hasActiveBuild else {
            completion()
            return
        }
        terminationAfterPause = completion
        stopCurrentBuild()
    }

    func reportShortcutReady() {
        shortcutReady = true
        shortcutStatus = "⌥Space is ready — no Accessibility permission required"
        if state == .idle {
            latestActivity = "Press ⌥Space to speak"
        }
    }

    func reportShortcutFailure(_ message: String) {
        shortcutReady = false
        shortcutStatus = message
        if state == .idle {
            latestActivity = message
        }
    }

    func reportSetupRequired() {
        shortcutReady = false
        shortcutStatus = "Complete AI and GitHub setup to enable ⌥Space"
        if state == .idle {
            latestActivity = "Complete the welcome setup before your first build"
        }
    }

    func reportExternalBuildFailure(_ message: String) {
        state = .failed(message)
        latestActivity = message
        if hud == nil {
            hud = RecordingHUD(
                onStop: { [weak self] in self?.stopCurrentBuild() },
                onRestart: { [weak self] in self?.restartVoiceCommand() }
            )
        }
        hud?.showFailure(message)
    }

    func toggleRecording() {
        if state == .building, recorder == nil, transcriptionProcess == nil {
            switch dialogueStage {
            case .awaitingRequestAnswer:
                beginDialogueRecording(purpose: .requestConfirmation)
                return
            case .awaitingProjectName:
                beginDialogueRecording(purpose: .projectName)
                return
            case .awaitingNameAnswer:
                beginDialogueRecording(purpose: .nameConfirmation)
                return
            default:
                if process != nil || bridgeBuildTask != nil {
                    beginAmendmentRecording()
                    return
                }
                break
            }
        }
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

    private func beginAmendmentRecording() {
        guard voiceReady else {
            latestActivity = "Built-in voice assets are not ready"
            return
        }
        let recorder = makeRecorder(for: .amendment)
        configureCallbacks(for: recorder)
        self.recorder = recorder
        beginRecording(with: recorder, purpose: .amendment)
    }

    func startRecording() {
        guard process == nil else { return }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .notDetermined:
            requestMicrophonePermission(startRecordingWhenGranted: true)
            return
        case .denied, .restricted:
            reportMicrophoneDenied()
            return
        case .authorized:
            microphoneDenied = false
        @unknown default:
            reportMicrophoneDenied()
            return
        }
        guard voiceReady else {
            state = .failed("Offline voice assets are not ready")
            latestActivity = "Reinstall the complete Fractal Voice application"
            return
        }
        if let recorder {
            beginRecording(with: recorder, purpose: transcriptionPurpose)
            return
        }

        if dialogueStage == .none {
            pendingRequest = ""
            pendingRequestWasTyped = false
            pendingProjectName = ""
            transcriptRetryCount = 0
            transcriptionPurpose = .request
        }
        state = .preparing
        latestActivity = "Starting Granite Speech and Kokoro locally…"
        hud = RecordingHUD(
            onStop: { [weak self] in self?.stopCurrentBuild() },
            onRestart: { [weak self] in self?.restartVoiceCommand() },
            onYes: { [weak self] in self?.answerConfirmation(true) },
            onNo: { [weak self] in self?.answerConfirmation(false) },
            onTypeInstead: { [weak self] in self?.beginManualRequestEntry() },
            onManualRequest: { [weak self] in self?.acceptManualRequest($0) },
            onManualName: { [weak self] in self?.acceptManualProjectName($0) }
        )
        hud?.showPreparing()
        let recorder = makeRecorder(for: .request)
        configureCallbacks(for: recorder)
        self.recorder = recorder
        beginRecording(with: recorder, purpose: .request)
    }

    func startExternalBuild(_ external: ExternalBuildRequest) throws {
        guard canAcceptExternalBuild else {
            throw ExternalBuildStartError.busy
        }
        guard Self.fractalExecutable() != nil else {
            throw ExternalBuildStartError.cliMissing
        }

        cancelDialogueInput()
        pendingRequest = external.request
        pendingRequestWasTyped = true
        pendingProjectName = external.projectName
        stopRequested = false
        restartRequested = false
        state = .building
        latestActivity = "External request received — starting \(external.projectName)…"
        hud?.close()
        hud = RecordingHUD(
            onStop: { [weak self] in self?.stopCurrentBuild() },
            onRestart: { [weak self] in self?.restartVoiceCommand() }
        )
        hud?.showBuilding(
            summary: "External request received — starting \(external.projectName)…"
        )
        startBuild(
            transcript: external.request,
            projectName: external.projectName,
            applyVoiceVocabulary: false
        )
    }

    func applyExternalVisibility(_ request: ExternalVisibilityRequest, resultURL: URL) {
        guard let executable = Self.fractalExecutable() else {
            let message = ExternalBuildStartError.cliMissing.localizedDescription
            reportExternalBuildFailure(message)
            Self.writeVisibilityResult(to: resultURL, success: false, message: message)
            return
        }
        latestActivity = "Updating \(request.target) visibility through GitHub…"
        let task = Process()
        let output = Pipe()
        task.executableURL = executable
        task.arguments = [
            "visibility",
            "--project", request.workspace,
            "--\(request.target)",
            "--yes",
        ]
        var environment = Self.processEnvironment()
        environment["FRACTAL_VISIBILITY_RECEIVER"] = "1"
        environment["PATH"] = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
        task.environment = environment
        task.standardOutput = output
        task.standardError = output
        task.terminationHandler = { [weak self] finished in
            let data = output.fileHandleForReading.readDataToEndOfFile()
            let message = String(data: data, encoding: .utf8)?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            Task { @MainActor in
                if finished.terminationStatus == 0 {
                    let success =
                        "Visibility updated: project graph and GitHub repository are now "
                        + "\(request.target)."
                    self?.latestActivity = success
                    Self.writeVisibilityResult(
                        to: resultURL,
                        success: true,
                        message: success
                    )
                } else {
                    let failure = message.isEmpty
                        ? "GitHub visibility update failed."
                        : message
                    self?.reportExternalBuildFailure(failure)
                    Self.writeVisibilityResult(
                        to: resultURL,
                        success: false,
                        message: failure
                    )
                }
            }
        }
        do {
            try task.run()
        } catch {
            reportExternalBuildFailure(error.localizedDescription)
            Self.writeVisibilityResult(
                to: resultURL,
                success: false,
                message: error.localizedDescription
            )
        }
    }

    nonisolated private static func writeVisibilityResult(
        to url: URL,
        success: Bool,
        message: String
    ) {
        let payload: [String: Any] = ["success": success, "message": message]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        FileManager.default.createFile(
            atPath: url.path,
            contents: data,
            attributes: [.posixPermissions: 0o600]
        )
    }

    func startWebsiteTask(token: String, server: URL, action: String) throws {
        guard canAcceptExternalBuild else {
            throw ExternalBuildStartError.busy
        }
        guard let executable = Self.fractalExecutable() else {
            throw ExternalBuildStartError.cliMissing
        }

        cancelDialogueInput()
        stopRequested = false
        restartRequested = false
        state = .building
        latestActivity = action == "resume"
            ? "Resume accepted — locating the saved project checkpoint…"
            : "Task accepted — preparing its dedicated review branch…"
        hud?.close()
        hud = RecordingHUD(
            onStop: { [weak self] in self?.stopCurrentBuild() },
            onRestart: { [weak self] in self?.restartVoiceCommand() }
        )
        hud?.showBuilding(summary: latestActivity)

        let task = Process()
        let combinedOutput = Pipe()
        task.executableURL = executable
        task.arguments = [
            "contribute",
            "--token", token,
            "--server", server.absoluteString,
        ]
        task.environment = Self.processEnvironment()
        task.standardOutput = combinedOutput
        task.standardError = combinedOutput
        outputBuffer = ""
        outputLineBuffer = ""
        activeWorkspace = nil
        combinedOutput.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor in self?.consume(text) }
        }
        task.terminationHandler = { [weak self] finished in
            combinedOutput.fileHandleForReading.readabilityHandler = nil
            Task { @MainActor in self?.finished(exitCode: finished.terminationStatus) }
        }
        do {
            try task.run()
            process = task
        } catch {
            recordingFailed(error)
        }
    }

    func requestMicrophonePermission(startRecordingWhenGranted: Bool = false) {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            microphoneDenied = false
            if startRecordingWhenGranted {
                startRecording()
            }
        case .denied, .restricted:
            reportMicrophoneDenied()
        case .notDetermined:
            state = .preparing
            latestActivity = "Fractal needs the microphone only to hear your spoken build request"
            let explanation = NSAlert()
            explanation.messageText = "Allow Microphone access?"
            explanation.informativeText =
                "Fractal Voice records only while the listening indicator is visible. "
                + "Granite Speech transcribes the recording locally on this Mac; "
                + "the microphone audio is not uploaded to a transcription service."
            explanation.alertStyle = .informational
            explanation.addButton(withTitle: "Continue")
            explanation.addButton(withTitle: "Not Now")
            guard explanation.runModal() == .alertFirstButtonReturn else {
                state = .idle
                latestActivity = "Microphone access was not requested — open the Fractal menu when you are ready"
                return
            }
            latestActivity = "macOS will now ask for Microphone access…"
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                Task { @MainActor in
                    guard let self else { return }
                    if granted {
                        self.microphoneDenied = false
                        self.state = .idle
                        self.latestActivity = "Microphone ready — press ⌥Space to speak"
                        if startRecordingWhenGranted {
                            self.startRecording()
                        }
                    } else {
                        self.reportMicrophoneDenied()
                    }
                }
            }
        @unknown default:
            reportMicrophoneDenied()
        }
    }

    private func beginRecording(
        with recorder: NativeVoiceRecorder,
        purpose: TranscriptionPurpose
    ) {
        do {
            try recorder.start()
            transcriptionPurpose = purpose
            state = .recording
            let summary: String
            switch purpose {
            case .request:
                summary = "Tell Fractal what you want to build — listening stops when you finish"
            case .amendment:
                summary = "Add to the active build — for example, “add to task 1.2 a branch that adds export”"
            case .requestConfirmation, .nameConfirmation:
                summary = "Just say yes or no — or use the buttons"
            case .projectName:
                summary = "Say the project name — listening stops when you finish"
            }
            latestActivity = summary
            if purpose == .projectName {
                hud?.showNaming(summary)
            } else if purpose == .request || purpose == .amendment {
                hud?.showListening(summary: summary)
            }
            scheduleRecordingTimeout(for: recorder, purpose: purpose)
        } catch {
            if purpose == .amendment {
                amendmentFailed(error)
            } else {
                state = .failed("Could not start microphone capture")
                latestActivity = error.localizedDescription
            }
        }
    }

    func stopRecordingAndBuild() {
        guard state == .recording, let recorder else { return }
        recordingTimeout?.cancel()
        recordingTimeout = nil
        state = .building
        latestActivity = "Finishing the local transcript…"
        if transcriptionPurpose == .requestConfirmation {
            hud?.showQuestion(Self.requestQuestion(pendingRequest))
        } else if transcriptionPurpose == .nameConfirmation {
            hud?.showQuestion(Self.nameQuestion(pendingProjectName))
        } else if transcriptionPurpose == .projectName {
            hud?.showNaming("Confirming the project name locally…")
        } else {
            hud?.showBuilding()
        }
        NSSound(named: "Pop")?.play()

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            do {
                let audioURL = try recorder.stop()
                Task { @MainActor in
                    self?.recorder = nil
                    self?.startGraniteTranscription(
                        audioURL: audioURL,
                        purpose: self?.transcriptionPurpose ?? .request
                    )
                }
            } catch {
                Task { @MainActor in
                    self?.recordingFailed(error)
                }
            }
        }
    }

    func stopAllBuilds() {
        #if APP_STORE
        Task.detached(priority: .userInitiated) {
            try? LocalBridge.stop(project: nil, all: true)
        }
        latestActivity = "Stop requested for all Fractal builds"
        return
        #else
        guard let executable = Self.fractalExecutable() else { return }
        let stop = Process()
        stop.executableURL = executable
        stop.arguments = ["stop", "--all"]
        stop.environment = Self.processEnvironment()
        try? stop.run()
        latestActivity = "Stop requested for all Fractal builds"
        #endif
    }

    func stopCurrentBuild() {
        requestBuildStop(restart: false)
    }

    func restartVoiceCommand() {
        requestBuildStop(restart: true)
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

    func openMicrophoneSettings() {
        guard let url = URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        ) else {
            return
        }
        NSWorkspace.shared.open(url)
    }

    private func reportMicrophoneDenied() {
        microphoneDenied = true
        state = .failed("Microphone access is off")
        latestActivity = "Enable Fractal Voice in System Settings → Privacy & Security → Microphone"
        hud?.close()
        hud = nil
    }

    private func configureCallbacks(for recorder: NativeVoiceRecorder) {
        recorder.onError = { [weak self] message in
            Task { @MainActor in
                self?.latestActivity = message
            }
        }
        recorder.onSpeechDetected = { [weak self, weak recorder] in
            guard let self, let recorder, self.recorder === recorder else { return }
            self.recordingTimeout?.cancel()
            self.recordingTimeout = nil
            self.latestActivity = "Got it — finish speaking naturally"
            if self.transcriptionPurpose == .projectName {
                self.hud?.showNaming("Got it — finish speaking naturally")
            }
        }
        recorder.onUtteranceEnded = { [weak self, weak recorder] in
            guard
                let self,
                let recorder,
                self.recorder === recorder,
                self.state == .recording
            else {
                return
            }
            self.latestActivity = "Speech complete — transcribing locally…"
            self.stopRecordingAndBuild()
        }
    }

    private func scheduleRecordingTimeout(
        for recorder: NativeVoiceRecorder,
        purpose: TranscriptionPurpose
    ) {
        recordingTimeout?.cancel()
        recordingTimeout = Task { [weak self, weak recorder] in
            try? await Task.sleep(nanoseconds: 60_000_000_000)
            guard
                !Task.isCancelled,
                let self,
                let recorder,
                self.recorder === recorder,
                self.state == .recording
            else {
                return
            }
            recorder.close()
            self.recorder = nil
            self.recordingTimeout = nil
            NSSound.beep()
            if purpose == .request, self.dialogueStage == .none {
                self.hud?.close()
                self.hud = nil
                self.state = .idle
                self.latestActivity = "No speech heard for 60 seconds — press ⌥Space to try again"
                return
            }
            self.state = .building
            self.latestActivity = "Microphone paused after 60 seconds — press ⌥Space to answer"
            if purpose == .requestConfirmation {
                self.hud?.showQuestion(Self.requestQuestion(self.pendingRequest))
            } else if purpose == .nameConfirmation {
                self.hud?.showQuestion(Self.nameQuestion(self.pendingProjectName))
            } else {
                self.hud?.showNaming("Microphone paused — press ⌥Space to say the project name")
            }
        }
    }

    private func startGraniteTranscription(
        audioURL: URL,
        purpose: TranscriptionPurpose
    ) {
        guard
            let executable = Self.graniteExecutable(),
            let assets = Self.graniteAssets()
        else {
            try? FileManager.default.removeItem(at: audioURL)
            recordingFailed(VoiceAppError.graniteMissing)
            return
        }
        if stopRequested {
            try? FileManager.default.removeItem(at: audioURL)
            finishRequestedStopBeforeBuild()
            return
        }

        let task = Process()
        let stdout = Pipe()
        task.executableURL = executable
        let warmServer = graniteServerProcess?.isRunning == true
            ? graniteServerBaseURL
            : nil
        task.arguments = Self.graniteTranscriptionArguments(
            audioURL: audioURL,
            prompt: granitePrompt(for: purpose),
            assets: assets,
            serverBaseURL: warmServer,
            shortAnswer: purpose == .requestConfirmation
                || purpose == .nameConfirmation
        )
        task.environment = Self.processEnvironment()
        task.standardInput = FileHandle.nullDevice
        task.standardOutput = stdout
        task.standardError = FileHandle.nullDevice
        let generation = dialogueGeneration
        task.terminationHandler = { [weak self] finished in
            let output = stdout.fileHandleForReading.readDataToEndOfFile()
            let text = String(data: output, encoding: .utf8) ?? ""
            Task { @MainActor in
                guard self?.dialogueGeneration == generation else { return }
                self?.graniteFinished(
                    output: text,
                    exitCode: finished.terminationStatus,
                    audioURL: audioURL,
                    purpose: purpose
                )
            }
        }

        do {
            try task.run()
            transcriptionProcess = task
            transcriptionStartedAt = Date()
            transcriptionUsedWarmServer = warmServer != nil
            activeAudioURL = audioURL
            latestActivity = "Granite is transcribing with Fractal vocabulary…"
            if purpose == .requestConfirmation {
                hud?.showQuestion(Self.requestQuestion(pendingRequest))
            } else if purpose == .nameConfirmation {
                hud?.showQuestion(Self.nameQuestion(pendingProjectName))
            } else if purpose == .projectName {
                hud?.showNaming("Confirming the project name locally…")
            } else {
                hud?.showTranscribing()
            }
        } catch {
            try? FileManager.default.removeItem(at: audioURL)
            if purpose == .amendment {
                amendmentFailed(error)
            } else {
                recordingFailed(error)
            }
        }
    }

    private func granitePrompt(for purpose: TranscriptionPurpose) -> String {
        switch purpose {
        case .requestConfirmation, .nameConfirmation:
            return "Transcribe the spoken answer. Return only yes or no when either is spoken."
        case .projectName:
            return "Transcribe the exact project name. Preserve spelling, numbers, and technical terms. Return only the name."
        case .request:
            break
        case .amendment:
            return "Transcribe the exact command for changing the active execution graph. Preserve task numbers such as 0.1 and 2.3. Return only the spoken command."
        }
        let terms = vocabularyEngine.promptTerms(projectURL: Self.activeProjectURL())
        let keywords = terms.prefix(96).joined(separator: ", ")
        return "transcribe the speech with proper punctuation and capitalization. "
            + "Return only the exact spoken instruction. "
            + "Keywords: \(keywords)"
    }

    private func graniteFinished(
        output: String,
        exitCode: Int32,
        audioURL: URL,
        purpose: TranscriptionPurpose
    ) {
        transcriptionProcess = nil
        if let startedAt = transcriptionStartedAt {
            let elapsed = Date().timeIntervalSince(startedAt)
            appendLog(
                String(
                    format: "[voice] transcription finished in %.2fs via %@\n",
                    elapsed,
                    transcriptionUsedWarmServer ? "warm Granite server" : "one-shot Granite"
                )
            )
        }
        transcriptionStartedAt = nil
        activeAudioURL = nil
        try? FileManager.default.removeItem(at: audioURL)
        if stopRequested {
            finishRequestedStopBeforeBuild()
            return
        }
        guard exitCode == 0 else {
            if purpose == .amendment {
                amendmentFailed(VoiceAppError.graniteFailed(exitCode))
            } else {
                recordingFailed(VoiceAppError.graniteFailed(exitCode))
            }
            return
        }
        let transcript = Self.cleanGraniteTranscript(output)
        guard !transcript.isEmpty, !Self.isLikelyGraniteHallucination(transcript) else {
            retryAfterUnusableTranscript(purpose: purpose)
            return
        }
        transcriptRetryCount = 0
        switch purpose {
        case .request:
            // Confirm the speech transcript exactly as heard. Vocabulary
            // normalization still runs immediately before the confirmed request
            // is sent to Fractal, but must not silently rewrite this question.
            pendingRequest = transcript
            pendingRequestWasTyped = false
            askToConfirmRequest()
        case .amendment:
            submitAmendment(transcript)
        case .requestConfirmation:
            handleSpokenConfirmation(transcript, forName: false)
        case .projectName:
            pendingProjectName = Self.cleanProjectName(transcript)
            guard !pendingProjectName.isEmpty else {
                askForProjectName()
                return
            }
            askToConfirmName()
        case .nameConfirmation:
            handleSpokenConfirmation(transcript, forName: true)
        }
    }

    private func retryAfterUnusableTranscript(purpose: TranscriptionPurpose) {
        transcriptRetryCount += 1
        appendLog("[voice] rejected an empty or known hallucinated transcript\n")
        guard transcriptRetryCount <= 2 else {
            if purpose == .amendment {
                amendmentFailed(VoiceAppError.noSpeech)
            } else {
                recordingFailed(VoiceAppError.noSpeech)
            }
            return
        }
        state = .building
        latestActivity = "I didn’t catch usable speech — listening again…"
        switch purpose {
        case .request:
            dialogueStage = .none
            hud?.showListening(summary: latestActivity)
        case .amendment:
            hud?.showListening(summary: "I didn’t catch the graph change — please say it again")
        case .requestConfirmation:
            dialogueStage = .awaitingRequestAnswer
            hud?.showQuestion(Self.requestQuestion(pendingRequest))
        case .projectName:
            dialogueStage = .awaitingProjectName
            hud?.showNaming("I didn’t catch the name — please say it again")
        case .nameConfirmation:
            dialogueStage = .awaitingNameAnswer
            hud?.showQuestion(Self.nameQuestion(pendingProjectName))
        }
        NSSound.beep()
        beginDialogueRecording(purpose: purpose)
    }

    private func askToConfirmRequest() {
        guard !pendingRequest.isEmpty else {
            recordingFailed(VoiceAppError.noSpeech)
            return
        }
        dialogueStage = .confirmingRequest
        state = .building
        let question = Self.requestQuestion(pendingRequest)
        latestActivity = question
        hud?.showQuestion(question)
        speakThenListen(
            question,
            expectedStage: .confirmingRequest,
            nextStage: .awaitingRequestAnswer,
            purpose: .requestConfirmation
        )
    }

    private func askForProjectName() {
        askForProjectName(prompt: "What do you want to call it?")
    }

    private func askForProjectName(prompt: String) {
        dialogueStage = .awaitingProjectName
        state = .building
        latestActivity = prompt
        hud?.showNaming(prompt)
        speakThenListen(
            prompt,
            expectedStage: .awaitingProjectName,
            nextStage: .awaitingProjectName,
            purpose: .projectName
        )
    }

    private func beginManualRequestEntry() {
        guard transcriptionPurpose == .request,
              state == .recording || state == .preparing else {
            return
        }
        cancelDialogueInput()
        dialogueStage = .none
        state = .building
        latestActivity = "Type exactly what you want Fractal to build, then press Enter"
        hud?.showManualRequest()
    }

    private func acceptManualRequest(_ request: String) {
        let request = request.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !request.isEmpty else { return }
        cancelDialogueInput()
        pendingRequest = request
        pendingRequestWasTyped = true
        pendingProjectName = ""
        state = .building
        // Typed input is already exact and user-approved, so skip the speech
        // transcript confirmation and move directly into project naming.
        askForProjectName()
    }

    private func acceptManualProjectName(_ name: String) {
        let name = Self.cleanManualProjectName(name)
        guard !name.isEmpty, !pendingRequest.isEmpty else { return }
        cancelDialogueInput()
        pendingProjectName = name
        startConfirmedBuild()
    }

    private func askToConfirmName() {
        dialogueStage = .confirmingName
        state = .building
        let question = Self.nameQuestion(pendingProjectName)
        latestActivity = question
        hud?.showQuestion(question)
        speakThenListen(
            question,
            expectedStage: .confirmingName,
            nextStage: .awaitingNameAnswer,
            purpose: .nameConfirmation
        )
    }

    private func speakThenListen(
        _ text: String,
        expectedStage: DialogueStage,
        nextStage: DialogueStage,
        purpose: TranscriptionPurpose
    ) {
        speaker.speak(text) { [weak self] result in
            guard let self, self.dialogueStage == expectedStage else { return }
            switch result {
            case .success:
                self.dialogueStage = nextStage
                self.beginDialogueRecording(purpose: purpose)
            case .failure(let error):
                self.recordingFailed(error)
            }
        }
    }

    private func beginDialogueRecording(purpose: TranscriptionPurpose) {
        let recorder = makeRecorder(for: purpose)
        configureCallbacks(for: recorder)
        self.recorder = recorder
        beginRecording(with: recorder, purpose: purpose)
    }

    private func makeRecorder(for purpose: TranscriptionPurpose) -> NativeVoiceRecorder {
        let endingSilence: Double
        switch purpose {
        case .requestConfirmation, .nameConfirmation:
            endingSilence = 0.42
        case .request, .projectName, .amendment:
            endingSilence = 0.62
        }
        return NativeVoiceRecorder(endingSilenceDuration: endingSilence)
    }

    private func submitAmendment(_ transcript: String) {
        state = .building
        latestActivity = "Sending graph change to the lead planner…"
        hud?.showBuilding(summary: latestActivity)
        #if APP_STORE
        Task { [weak self] in
            do {
                let result = try await Task.detached(priority: .userInitiated) {
                    try LocalBridge.amend(transcript)
                }.value
                guard let self else { return }
                let message = result.output
                    .split(separator: "\n")
                    .last
                    .map(String.init) ?? "Branch request accepted"
                self.setActivity(message)
            } catch {
                self?.setActivity("Branch request was not accepted: \(error.localizedDescription)")
            }
        }
        #else
        guard let executable = Self.fractalExecutable() else {
            setActivity("Fractal CLI is missing")
            return
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let command = Process()
            let stdin = Pipe()
            let output = Pipe()
            command.executableURL = executable
            command.arguments = [
                "ingest", "--source", "fractal-mac-app",
                "--format", "text", "--stdin"
            ]
            command.environment = Self.processEnvironment()
            command.standardInput = stdin
            command.standardOutput = output
            command.standardError = output
            do {
                try command.run()
                stdin.fileHandleForWriting.write(Data(transcript.utf8))
                try stdin.fileHandleForWriting.close()
                command.waitUntilExit()
                let message = String(
                    decoding: output.fileHandleForReading.readDataToEndOfFile(),
                    as: UTF8.self
                ).trimmingCharacters(in: .whitespacesAndNewlines)
                Task { @MainActor in
                    self?.setActivity(message.isEmpty ? "Branch request accepted" : message)
                }
            } catch {
                Task { @MainActor in
                    self?.setActivity("Branch request was not accepted: \(error.localizedDescription)")
                }
            }
        }
        #endif
    }

    private func amendmentFailed(_ error: Error) {
        recordingTimeout?.cancel()
        recordingTimeout = nil
        transcriptionProcess = nil
        recorder?.close()
        recorder = nil
        if let activeAudioURL {
            try? FileManager.default.removeItem(at: activeAudioURL)
        }
        activeAudioURL = nil
        state = .building
        latestActivity = "Graph change was not accepted: \(error.localizedDescription)"
        hud?.showBuilding(summary: latestActivity)
    }

    private func handleSpokenConfirmation(_ transcript: String, forName: Bool) {
        guard let answer = Self.yesNoAnswer(transcript) else {
            let prompt = "Please answer yes or no."
            latestActivity = prompt
            hud?.showQuestion(forName
                ? Self.nameQuestion(pendingProjectName)
                : Self.requestQuestion(pendingRequest))
            dialogueStage = forName ? .confirmingName : .confirmingRequest
            speaker.speak(prompt) { [weak self] result in
                guard let self else { return }
                if case .failure(let error) = result {
                    self.recordingFailed(error)
                    return
                }
                self.dialogueStage = forName ? .awaitingNameAnswer : .awaitingRequestAnswer
                self.beginDialogueRecording(
                    purpose: forName ? .nameConfirmation : .requestConfirmation
                )
            }
            return
        }
        answerConfirmation(answer)
    }

    private func answerConfirmation(_ yes: Bool) {
        let answeringName = dialogueStage == .confirmingName
            || dialogueStage == .awaitingNameAnswer
        let answeringRequest = dialogueStage == .confirmingRequest
            || dialogueStage == .awaitingRequestAnswer
        guard answeringName || answeringRequest else { return }
        cancelDialogueInput()

        if answeringRequest {
            if yes {
                askForProjectName()
            } else {
                dialogueStage = .none
                pendingRequest = ""
                pendingRequestWasTyped = false
                pendingProjectName = ""
                state = .building
                hud?.showListening(summary: "Okay — tell me what you want to build instead")
                speaker.speak("Okay. Tell me what you want to build instead.") { [weak self] result in
                    guard let self, self.dialogueStage == .none else { return }
                    if case .failure(let error) = result {
                        self.recordingFailed(error)
                        return
                    }
                    self.beginDialogueRecording(purpose: .request)
                }
            }
            return
        }

        if yes {
            startConfirmedBuild()
        } else {
            pendingProjectName = ""
            askForProjectName()
        }
    }

    private func startConfirmedBuild() {
        dialogueStage = .none
        let request = pendingRequest
        let requestWasTyped = pendingRequestWasTyped
        let projectName = pendingProjectName
        state = .building
        hud?.showBuilding(summary: "Confirmed — starting \(projectName)…")
        latestActivity = "Confirmed — starting \(projectName)…"
        // Launch immediately. The acknowledgement is non-blocking and never
        // delays the planner.
        speaker.speak("Great. I am starting \(projectName) now.") { _ in }
        startBuild(
            transcript: request,
            projectName: projectName,
            applyVoiceVocabulary: !requestWasTyped
        )
    }

    private func cancelDialogueInput() {
        recordingTimeout?.cancel()
        recordingTimeout = nil
        dialogueGeneration += 1
        speaker.stop()
        recorder?.close()
        recorder = nil
        if let transcriptionProcess {
            transcriptionProcess.terminationHandler = nil
            transcriptionProcess.terminate()
            self.transcriptionProcess = nil
        }
        if let activeAudioURL {
            try? FileManager.default.removeItem(at: activeAudioURL)
        }
        activeAudioURL = nil
    }

    nonisolated static func yesNoAnswer(_ transcript: String) -> Bool? {
        let words = transcript.lowercased()
            .split(whereSeparator: { !$0.isLetter })
            .map(String.init)
        let yesWords: Set<String> = ["yes", "yep", "yeah", "correct", "right", "affirmative"]
        let noWords: Set<String> = ["no", "nope", "negative", "incorrect", "wrong"]
        if words.contains(where: yesWords.contains) { return true }
        if words.contains(where: noWords.contains) { return false }
        return nil
    }

    nonisolated static func cleanProjectName(_ transcript: String) -> String {
        compact(
            transcript.trimmingCharacters(in: CharacterSet(
                charactersIn: " \t\r\n\"“”'‘’.,!?"
            )),
            limit: 80
        )
    }

    nonisolated static func cleanManualProjectName(_ typedName: String) -> String {
        compact(
            typedName.trimmingCharacters(in: .whitespacesAndNewlines),
            limit: 80
        )
    }

    nonisolated static func requestQuestion(_ request: String) -> String {
        let transcript = request.trimmingCharacters(in: .whitespacesAndNewlines)
        return "You said, “\(transcript)”. Is that correct?"
    }

    nonisolated static func nameQuestion(_ name: String) -> String {
        "Okay, I will call it “\(compact(name, limit: 80))”. Is that correct?"
    }

    private func startBuild(
        transcript: String,
        projectName: String,
        applyVoiceVocabulary: Bool
    ) {
        let vocabularyResult = applyVoiceVocabulary
            ? vocabularyEngine.normalize(
                transcript,
                projectURL: Self.activeProjectURL()
            )
            : nil
        let transcript = vocabularyResult?.transcript ?? transcript
        if stopRequested {
            finishRequestedStopBeforeBuild()
            return
        }
        guard !transcript.isEmpty else {
            recordingFailed(VoiceAppError.noSpeech)
            return
        }
        #if APP_STORE
        startBridgeBuild(transcript: transcript, projectName: projectName)
        if vocabularyResult?.appliedCorrections.isEmpty ?? true {
            setActivity("Heard: “\(Self.compact(transcript, limit: 180))”")
        } else {
            appendLog(
                "[voice] applied \(vocabularyResult?.appliedCorrections.count ?? 0) "
                + "local vocabulary correction(s)\n"
            )
            setActivity("Understood: “\(Self.compact(transcript, limit: 180))”")
        }
        return
        #else
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
            "--managed-project",
            "--project-name", projectName
        ]
        task.environment = Self.processEnvironment()
        task.standardInput = stdin
        task.standardOutput = combinedOutput
        task.standardError = combinedOutput

        outputBuffer = ""
        outputLineBuffer = ""
        activeWorkspace = nil
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
            if vocabularyResult?.appliedCorrections.isEmpty ?? true {
                setActivity("Heard: “\(Self.compact(transcript, limit: 180))”")
            } else {
                appendLog(
                    "[voice] applied \(vocabularyResult?.appliedCorrections.count ?? 0) "
                    + "local vocabulary correction(s)\n"
                )
                setActivity("Understood: “\(Self.compact(transcript, limit: 180))”")
            }
        } catch {
            recordingFailed(error)
        }
        #endif
    }

    #if APP_STORE
    private func startBridgeBuild(transcript: String, projectName: String) {
        outputBuffer = ""
        outputLineBuffer = ""
        activeWorkspace = nil
        bridgeBuildTask?.cancel()
        bridgeBuildTask = Task { [weak self] in
            do {
                let result = try await Task.detached(priority: .userInitiated) {
                    try LocalBridge.build(request: transcript, projectName: projectName)
                }.value
                guard !Task.isCancelled, let self else { return }
                self.consume(result.output)
                self.finished(exitCode: result.exitCode)
            } catch is CancellationError {
                return
            } catch {
                guard !Task.isCancelled else { return }
                self?.recordingFailed(error)
            }
        }
    }
    #endif

    private func recordingFailed(_ error: Error) {
        recordingTimeout?.cancel()
        recordingTimeout = nil
        dialogueGeneration += 1
        dialogueStage = .none
        speaker.stop()
        process = nil
        transcriptionProcess?.terminate()
        transcriptionProcess = nil
        if let activeAudioURL {
            try? FileManager.default.removeItem(at: activeAudioURL)
        }
        activeAudioURL = nil
        recorder?.close()
        recorder = nil
        state = .failed("Voice command stopped")
        latestActivity = error.localizedDescription
        appendLog("[voice] command failed: \(error.localizedDescription)\n")
        hud?.showFailure(error.localizedDescription)
        notify(title: "Fractal Voice needs attention", body: latestActivity)
    }

    private func consume(_ text: String) {
        appendLog(text)
        outputBuffer += text
        if outputBuffer.count > 32_000 {
            outputBuffer.removeFirst(outputBuffer.count - 32_000)
        }
        outputLineBuffer += text.replacingOccurrences(of: "\r", with: "\n")
        let lines = outputLineBuffer.split(
            separator: "\n",
            omittingEmptySubsequences: false
        )
        outputLineBuffer = String(lines.last ?? "")
        for line in lines.dropLast() {
            consumeLine(String(line))
        }
    }

    private func consumeLine(_ rawLine: String) {
        let line = Self.cleanTerminalLine(rawLine)
        guard !line.isEmpty else { return }
        if let prefix = line.range(of: "Created managed voice project:") {
            let path = line[prefix.upperBound...].trimmingCharacters(in: .whitespaces)
            if !path.isEmpty {
                activeWorkspace = URL(fileURLWithPath: path, isDirectory: true)
            }
        }
        if let summary = Self.activitySummary(for: line) {
            setActivity(summary)
        }
    }

    private func setActivity(_ activity: String) {
        latestActivity = activity
        if state == .building {
            hud?.updateBuilding(summary: activity)
        }
    }

    private func requestBuildStop(restart: Bool) {
        if dialogueStage != .none || (state == .recording && transcriptionPurpose != .request) {
            cancelDialogueInput()
            dialogueStage = .none
            pendingRequest = ""
            pendingRequestWasTyped = false
            pendingProjectName = ""
            hud?.close()
            hud = nil
            state = .idle
            latestActivity = restart
                ? "Previous voice attempt cleared — listening again…"
                : "Voice command cancelled before the build started"
            if restart {
                startRecording()
            }
            return
        }
        guard state == .building else {
            if restart, state != .preparing, state != .recording {
                startRecording()
            }
            return
        }
        stopRequested = true
        restartRequested = restart
        let activity = restart
            ? "Stopping this attempt — the microphone will reopen…"
            : "Pausing this build and preserving its completed work…"
        latestActivity = activity
        hud?.showStopping(restarting: restart)

        if let transcribing = transcriptionProcess {
            transcribing.terminate()
            return
        }
        #if APP_STORE
        Task.detached(priority: .userInitiated) {
            try? LocalBridge.stop(project: nil, all: false)
        }
        return
        #else
        guard let running = process else {
            return
        }
        guard let executable = Self.fractalExecutable() else {
            running.terminate()
            return
        }
        let stop = Process()
        stop.executableURL = executable
        if let activeWorkspace {
            stop.arguments = ["stop", "--project", activeWorkspace.path]
        } else {
            // Before the CLI announces its managed workspace there is no graph
            // checkpoint to preserve, so only terminate this known child.
            running.terminate()
            return
        }
        stop.environment = Self.processEnvironment()
        stop.standardInput = FileHandle.nullDevice
        stop.standardOutput = FileHandle.nullDevice
        stop.standardError = FileHandle.nullDevice
        stop.terminationHandler = { [weak self] finished in
            let failed = finished.terminationStatus != 0
            Task { @MainActor in
                if failed, self?.process?.isRunning == true {
                    self?.process?.terminate()
                }
                self?.stopCommand = nil
            }
        }
        do {
            try stop.run()
            stopCommand = stop
        } catch {
            running.terminate()
        }
        #endif
    }

    private func finishRequestedStopBeforeBuild() {
        let restart = restartRequested
        stopRequested = false
        restartRequested = false
        recorder?.close()
        recorder = nil
        if let activeAudioURL {
            try? FileManager.default.removeItem(at: activeAudioURL)
        }
        activeAudioURL = nil
        if restart {
            hud?.close()
            hud = nil
            state = .idle
            startRecording()
        } else {
            hud?.close()
            hud = nil
            state = .idle
            latestActivity = "Voice command cancelled before the build started"
        }
    }

    private func finished(exitCode: Int32) {
        process = nil
        stopCommand = nil
        bridgeBuildTask = nil
        if stopRequested {
            let restart = restartRequested
            stopRequested = false
            restartRequested = false
            activeWorkspace = nil
            hud?.close()
            hud = nil
            state = .idle
            if restart {
                latestActivity = "Previous attempt stopped — listening again…"
                startRecording()
            } else {
                latestActivity = "Build paused — completed work can be resumed later"
                notify(title: "Fractal build paused", body: latestActivity)
            }
            let completion = terminationAfterPause
            terminationAfterPause = nil
            completion?()
            return
        }
        if exitCode != 0, Self.projectNameWasTaken(outputBuffer) {
            activeWorkspace = nil
            let takenName = pendingProjectName
            pendingProjectName = ""
            askForProjectName(
                prompt: "You already have a project called \(takenName). What would you like to call this one?"
            )
            return
        }
        hud?.close()
        hud = nil
        if exitCode == 0 {
            state = .idle
            latestActivity = "Build finished — press ⌥Space for another project"
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

    nonisolated static func activitySummary(for rawLine: String) -> String? {
        let line = cleanTerminalLine(rawLine)
        guard !line.isEmpty else { return nil }
        if line.contains("Created managed voice project:") {
            return "Project created — lead agent is preparing the plan…"
        }
        if line.contains("Interpreted instruction:") {
            return "Instruction understood — creating the project…"
        }
        if line.contains("opening the project now") || line.contains("Planning graph:") {
            return "Execution graph is live — lead planning is underway…"
        }
        if line.contains("⏳ [") {
            return compact(line, limit: 220)
        }
        if line.contains("lead proposed") || line.contains("compiling the execution graph") {
            return compact(line, limit: 220)
        }
        if line.contains("executing with") || line.contains("→ executing in") {
            return "Workers are building from the execution graph…"
        }
        if line.contains("✓") || line.contains("✗") {
            return compact(line, limit: 220)
        }
        return nil
    }

    nonisolated static func projectNameWasTaken(_ output: String) -> Bool {
        output.contains("FRACTAL_PROJECT_NAME_TAKEN:")
    }

    nonisolated static func cleanTerminalLine(_ line: String) -> String {
        line.replacingOccurrences(
            of: "\u{001B}\\[[0-9;?]*[ -/]*[@-~]",
            with: "",
            options: .regularExpression
        )
        .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    nonisolated static func compact(_ text: String, limit: Int) -> String {
        let singleLine = text
            .split(whereSeparator: { $0.isWhitespace })
            .joined(separator: " ")
        guard singleLine.count > limit else { return singleLine }
        return "\(singleLine.prefix(max(1, limit - 1)))…"
    }

    nonisolated static func cleanGraniteTranscript(_ output: String) -> String {
        let cleaned = output
            .replacingOccurrences(of: "<|end_of_text|>", with: "")
            .replacingOccurrences(of: "<|endoftext|>", with: "")
            .replacingOccurrences(of: "<|assistant|>", with: "")
            .replacingOccurrences(of: "<|response|>", with: "")
            .replacingOccurrences(of: "\r", with: "\n")
        let response: String
        if let promptMarker = cleaned.range(of: "\n> ") {
            let afterPrompt = cleaned[promptMarker.upperBound...]
            if let promptEnd = afterPrompt.firstIndex(of: "\n") {
                response = String(afterPrompt[afterPrompt.index(after: promptEnd)...])
            } else {
                response = ""
            }
        } else {
            response = cleaned
        }
        return response
            .split(separator: "\n")
            .map { cleanTerminalLine(String($0)) }
            .filter {
                !$0.isEmpty
                    && !$0.hasPrefix("llama_")
                    && !$0.hasPrefix("main:")
                    && !$0.hasPrefix("system_info:")
                    && !$0.hasPrefix("[ Prompt:")
                    && $0 != "Exiting..."
            }
            .joined(separator: " ")
            .trimmingCharacters(in: CharacterSet(
                charactersIn: " \t\r\n\"“”"
            ))
    }

    nonisolated static func isLikelyGraniteHallucination(_ transcript: String) -> Bool {
        let normalized = transcript
            .lowercased()
            .components(separatedBy: CharacterSet.alphanumerics.inverted)
            .filter { !$0.isEmpty }
            .joined(separator: " ")
        let known: Set<String> = [
            "thanks for watching",
            "thank you for watching",
            "thanks for listening",
            "thank you for listening",
            "please subscribe",
            "like and subscribe",
            "subtitles by",
            "all right",
            "alright",
            "the end",
            "music",
            "applause",
        ]
        return normalized.isEmpty || known.contains(normalized)
    }

    nonisolated static func graniteTranscriptionArguments(
        audioURL: URL,
        prompt: String,
        assets: (model: URL, projector: URL),
        serverBaseURL: URL?,
        shortAnswer: Bool
    ) -> [String] {
        var arguments: [String]
        if let serverBaseURL {
            arguments = ["--server-base", serverBaseURL.absoluteString]
        } else {
            arguments = [
                "--model", assets.model.path,
                "--mmproj", assets.projector.path
            ]
        }
        arguments += [
            "--audio", audioURL.path,
            "--prompt", prompt,
            "--temp", "0",
            "--ctx-size", "4096",
            "--n-predict", shortAnswer ? "16" : "512",
            "--single-turn",
            "--simple-io",
            "--no-display-prompt",
            "--no-warmup",
            "--no-perf",
            "--log-disable"
        ]
        return arguments
    }

    private func notify(title: String, body: String) {
        Task { @MainActor [weak self] in
            guard self != nil else { return }
            let center = UNUserNotificationCenter.current()
            let settings = await center.notificationSettings()
            switch settings.authorizationStatus {
            case .notDetermined:
                let explanation = NSAlert()
                explanation.messageText = "Allow build notifications?"
                explanation.informativeText =
                    "Fractal uses notifications only to tell you when a build finishes "
                    + "or needs attention while its window is in the background. "
                    + "Notifications never expose credentials or grant access to your files."
                explanation.alertStyle = .informational
                explanation.addButton(withTitle: "Continue")
                explanation.addButton(withTitle: "Not Now")
                guard explanation.runModal() == .alertFirstButtonReturn else { return }
                guard (try? await center.requestAuthorization(options: [.alert, .sound])) == true
                else { return }
                Self.deliverNotification(title: title, body: body)
            case .authorized, .provisional, .ephemeral:
                Self.deliverNotification(title: title, body: body)
            case .denied:
                return
            @unknown default:
                return
            }
        }
    }

    nonisolated private static func deliverNotification(
        title: String,
        body: String
    ) {
        let center = UNUserNotificationCenter.current()
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

    nonisolated static func graniteAssets() -> (model: URL, projector: URL)? {
        let local = VoiceModelManager.graniteDirectory
        let bundled = Bundle.main.resourceURL?.appendingPathComponent(
            "GraniteModels/granite-speech-4.1-2b-q4", isDirectory: true
        )
        let directory = [local, bundled].compactMap { $0 }.first { directory in
            FileManager.default.fileExists(
                atPath: directory.appendingPathComponent(
                    "granite-speech-4.1-2b-Q4_K_M.gguf"
                ).path
            ) && FileManager.default.fileExists(
                atPath: directory.appendingPathComponent("mmproj-model-f16.gguf").path
            )
        }
        guard let directory else { return nil }
        let model = directory.appendingPathComponent(
            "granite-speech-4.1-2b-Q4_K_M.gguf"
        )
        let projector = directory.appendingPathComponent("mmproj-model-f16.gguf")
        guard
            FileManager.default.fileExists(atPath: model.path),
            FileManager.default.fileExists(atPath: projector.path)
        else {
            return nil
        }
        return (model, projector)
    }

    nonisolated static func graniteExecutable() -> URL? {
        guard let executable = Bundle.main.resourceURL?
            .appendingPathComponent("Granite/bin/llama-cli")
        else {
            return nil
        }
        return FileManager.default.isExecutableFile(atPath: executable.path)
            ? executable
            : nil
    }

    nonisolated static func graniteServerExecutable() -> URL? {
        guard let executable = Bundle.main.resourceURL?
            .appendingPathComponent("Granite/bin/llama-server")
        else {
            return nil
        }
        return FileManager.default.isExecutableFile(atPath: executable.path)
            ? executable
            : nil
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
        let home = AppRuntime.homeURL.path
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
        environment["FRACTAL_PROJECTS_DIR"] = AppRuntime.projectsURL.path
        let lead = UserDefaults.standard.string(forKey: "selectedLeadAgent")?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let lead, ["codex", "cursor", "claude", "hermes"].contains(lead) {
            environment["FRACTAL_LEAD_AGENT"] = lead
        }
        if AppRuntime.isAppStoreEdition {
            environment["FRACTAL_HOME"] = AppRuntime.applicationSupportURL
                .appendingPathComponent("CLI", isDirectory: true)
                .path
        }
        return environment
    }

    nonisolated static func activeProjectURL() -> URL? {
        let environment = ProcessInfo.processInfo.environment
        if let configured = environment["FRACTAL_PROJECT_DIR"], !configured.isEmpty {
            return URL(fileURLWithPath: configured, isDirectory: true)
        }
        let current = URL(
            fileURLWithPath: FileManager.default.currentDirectoryPath,
            isDirectory: true
        )
        let marker = current.appendingPathComponent(".fractal", isDirectory: true)
        guard FileManager.default.fileExists(atPath: marker.path) else {
            return nil
        }
        return current
    }
}

private enum VoiceAppError: LocalizedError {
    case noSpeech
    case cliMissing
    case graniteMissing
    case graniteFailed(Int32)

    var errorDescription: String? {
        switch self {
        case .noSpeech: return "No speech was detected. Press the shortcut and try again."
        case .cliMissing: return "The bundled Fractal CLI is missing."
        case .graniteMissing: return "The bundled Granite Speech engine is missing."
        case .graniteFailed(let code):
            return "Granite Speech could not transcribe this recording (exit \(code))."
        }
    }
}

enum ExternalBuildStartError: LocalizedError {
    case busy
    case cliMissing

    var errorDescription: String? {
        switch self {
        case .busy:
            return "Fractal Voice is already recording or building another project."
        case .cliMissing:
            return "The bundled Fractal CLI is unavailable."
        }
    }
}
