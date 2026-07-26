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
    @Published private(set) var latestActivity = "Checking bundled offline assets…"
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
    private var stopRequested = false
    private var restartRequested = false
    private var activeWorkspace: URL?
    private let vocabularyEngine = VoiceVocabularyEngine()
    private let speaker = KokoroSpeaker()
    private var transcriptionPurpose: TranscriptionPurpose = .request
    private var dialogueStage: DialogueStage = .none
    private var pendingRequest = ""
    private var pendingRequestWasTyped = false
    private var pendingProjectName = ""
    private var dialogueGeneration = 0
    private var recordingTimeout: Task<Void, Never>?

    let projectsURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("fractal-projects", isDirectory: true)
    let logURL = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/FractalVoice.log")

    init() {
        try? FileManager.default.createDirectory(
            at: projectsURL,
            withIntermediateDirectories: true
        )
        try? vocabularyEngine.installPersonalTemplateIfNeeded()
        voiceReady = Self.graniteAssets() != nil
            && Self.graniteExecutable() != nil
            && (try? KokoroSpeaker.assets()) != nil
            && Self.fractalExecutable() != nil
        latestActivity = voiceReady
            ? "Press ⌥Space to speak"
            : "Offline assets are missing — reinstall Fractal Voice"
        if voiceReady {
            startGraniteServer()
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
        let port = 18_371
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
        graniteServerBaseURL = nil
        if graniteServerProcess?.isRunning == true {
            graniteServerProcess?.terminate()
        }
        graniteServerProcess = nil
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

    func startRecording() {
        guard process == nil else { return }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .notDetermined:
            state = .preparing
            latestActivity = "Allow Microphone access to use Fractal Voice…"
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                Task { @MainActor in
                    guard let self else { return }
                    if granted {
                        self.microphoneDenied = false
                        self.state = .idle
                        self.startRecording()
                    } else {
                        self.reportMicrophoneDenied()
                    }
                }
            }
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
            case .requestConfirmation, .nameConfirmation:
                summary = "Just say yes or no — or use the buttons"
            case .projectName:
                summary = "Say the project name — listening stops when you finish"
            }
            latestActivity = summary
            if purpose == .projectName {
                hud?.showNaming(summary)
            } else if purpose == .request {
                hud?.showListening(summary: summary)
            }
            scheduleRecordingTimeout(for: recorder, purpose: purpose)
            NSSound(named: "Tink")?.play()
        } catch {
            state = .failed("Could not start microphone capture")
            latestActivity = error.localizedDescription
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
        guard let executable = Self.fractalExecutable() else { return }
        let stop = Process()
        stop.executableURL = executable
        stop.arguments = ["stop", "--all"]
        stop.environment = Self.processEnvironment()
        try? stop.run()
        latestActivity = "Stop requested for all Fractal builds"
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
            recordingFailed(error)
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
            recordingFailed(VoiceAppError.graniteFailed(exitCode))
            return
        }
        let transcript = Self.cleanGraniteTranscript(output)
        guard !transcript.isEmpty else {
            recordingFailed(VoiceAppError.noSpeech)
            return
        }
        switch purpose {
        case .request:
            // Confirm the speech transcript exactly as heard. Vocabulary
            // normalization still runs immediately before the confirmed request
            // is sent to Fractal, but must not silently rewrite this question.
            pendingRequest = transcript
            pendingRequestWasTyped = false
            askToConfirmRequest()
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
        case .request, .projectName:
            endingSilence = 0.62
        }
        return NativeVoiceRecorder(endingSilenceDuration: endingSilence)
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
    }

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

    nonisolated static func graniteAssets() -> (model: URL, projector: URL)? {
        guard let resources = Bundle.main.resourceURL else {
            return nil
        }
        let directory = resources.appendingPathComponent(
            "GraniteModels/granite-speech-4.1-2b-q4",
            isDirectory: true
        )
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
