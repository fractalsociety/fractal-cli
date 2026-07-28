import Carbon.HIToolbox
import XCTest
@testable import FractalVoice

final class FractalVoiceTests: XCTestCase {
    func testShortcutIsVisibleAndStable() {
        XCTAssertEqual(GlobalHotKey.displayName, "⌥Space")
        XCTAssertEqual(GlobalHotKey.keyCode, UInt32(kVK_Space))
        XCTAssertEqual(GlobalHotKey.modifiers, UInt32(optionKey))
    }

    func testVoiceActivityEndsAfterSpeechFollowedByNaturalSilence() {
        var detector = VoiceActivityDetector()
        XCTAssertNil(detector.observe(rms: 0.03, duration: 0.08))
        XCTAssertEqual(detector.observe(rms: 0.03, duration: 0.08), .speechStarted)
        for _ in 0..<7 {
            XCTAssertNil(detector.observe(rms: 0.001, duration: 0.1))
        }
        XCTAssertEqual(
            detector.observe(rms: 0.001, duration: 0.1),
            .utteranceEnded
        )
    }

    func testVoiceActivityRecognizesAQuietShortYesFromItsPeak() {
        var detector = VoiceActivityDetector()
        XCTAssertNil(detector.observe(rms: 0.002, peak: 0.04, duration: 0.08))
        XCTAssertEqual(
            detector.observe(rms: 0.002, peak: 0.04, duration: 0.08),
            .speechStarted
        )
        for _ in 0..<7 {
            XCTAssertNil(
                detector.observe(rms: 0.0008, peak: 0.001, duration: 0.1)
            )
        }
        XCTAssertEqual(
            detector.observe(rms: 0.0008, peak: 0.001, duration: 0.1),
            .utteranceEnded
        )
    }

    func testShortAnswerVoiceActivityUsesAQuickerNaturalPause() {
        var detector = VoiceActivityDetector(endingSilenceDuration: 0.42)
        XCTAssertNil(detector.observe(rms: 0.02, duration: 0.08))
        XCTAssertEqual(detector.observe(rms: 0.02, duration: 0.08), .speechStarted)
        for _ in 0..<4 {
            XCTAssertNil(detector.observe(rms: 0.0008, duration: 0.1))
        }
        XCTAssertEqual(
            detector.observe(rms: 0.0008, duration: 0.1),
            .utteranceEnded
        )
    }

    func testVoiceActivityDoesNotTreatRoomNoiseAsAnAnswer() {
        var detector = VoiceActivityDetector()
        for _ in 0..<100 {
            XCTAssertNil(detector.observe(rms: 0.002, duration: 0.1))
        }
        XCTAssertFalse(detector.heardSpeech)
    }

    func testVoiceActivityCalibrationIgnoresStartupToneAndLearnsRoomNoise() {
        var detector = VoiceActivityDetector(calibrationDuration: 0.25)
        XCTAssertNil(detector.observe(rms: 0.02, peak: 0.08, duration: 0.1))
        XCTAssertNil(detector.observe(rms: 0.003, peak: 0.01, duration: 0.1))
        XCTAssertNil(detector.observe(rms: 0.003, peak: 0.01, duration: 0.1))
        XCTAssertFalse(detector.heardSpeech)
        XCTAssertNil(detector.observe(rms: 0.03, peak: 0.12, duration: 0.08))
        XCTAssertEqual(
            detector.observe(rms: 0.03, peak: 0.12, duration: 0.08),
            .speechStarted
        )
    }

    func testProcessPathIncludesAgentInstallLocations() {
        let path = BuildCoordinator.processEnvironment()["PATH"] ?? ""
        XCTAssertTrue(path.contains(".cargo/bin"))
        XCTAssertTrue(path.contains(".local/bin"))
        XCTAssertTrue(path.contains("/opt/homebrew/bin"))
    }

    func testProjectLocationDefaultsAndPersistsAChosenFolder() throws {
        let suite = "FractalVoiceTests.ProjectsDirectory.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fractal-project-location-\(UUID().uuidString)")
        defer {
            defaults.removePersistentDomain(forName: suite)
            try? FileManager.default.removeItem(at: root)
        }

        XCTAssertEqual(
            AppRuntime.projectsURL(in: defaults).standardizedFileURL,
            AppRuntime.defaultProjectsURL.standardizedFileURL
        )
        try AppRuntime.configureProjectsURL(
            root,
            defaults: defaults,
            agentInstructions: "# Fractal Agent Operating Contract\n"
        )
        XCTAssertEqual(
            AppRuntime.projectsURL(in: defaults).standardizedFileURL,
            root.standardizedFileURL
        )
    }

    func testGlobalAgentInstructionsAreCreatedWithoutOverwritingUserChanges() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fractal-global-agents-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }

        try AppRuntime.installGlobalAgentInstructions(
            at: root,
            contents: "# Fractal Agent Operating Contract\n"
        )
        let destination = root.appendingPathComponent("AGENTS.md")
        XCTAssertEqual(
            try String(contentsOf: destination, encoding: .utf8),
            "# Fractal Agent Operating Contract\n"
        )

        try "user customization".write(
            to: destination,
            atomically: true,
            encoding: .utf8
        )
        try AppRuntime.installGlobalAgentInstructions(
            at: root,
            contents: "replacement"
        )
        XCTAssertEqual(
            try String(contentsOf: destination, encoding: .utf8),
            "user customization"
        )
    }

    func testManagedGlobalAgentInstructionsRefreshToTheBundledContract() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("fractal-refresh-agents-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(
            at: root,
            withIntermediateDirectories: true
        )
        let destination = root.appendingPathComponent("AGENTS.md")
        try "# Fractal Agent Operating Contract\nold instructions\n".write(
            to: destination,
            atomically: true,
            encoding: .utf8
        )

        try AppRuntime.installGlobalAgentInstructions(
            at: root,
            contents: "# Fractal Agent Operating Contract\nnew instructions\n"
        )

        XCTAssertEqual(
            try String(contentsOf: destination, encoding: .utf8),
            "# Fractal Agent Operating Contract\nnew instructions\n"
        )
    }

    func testSetupRequiresOneAuthenticatedAgentAndGitHub() {
        var agents = SetupReadiness.agentTemplates
        agents[0].installed = true
        agents[0].authenticated = true
        let ready = SetupSnapshot(
            agents: agents,
            gitInstalled: true,
            githubCLIInstalled: true,
            githubAuthenticated: true,
            fractalCLIInstalled: true,
            fractalSocietyAuthenticated: true,
            fractalSocietyAccount: "@builder"
        )
        XCTAssertTrue(ready.hasReadyAgent)
        XCTAssertTrue(ready.isReady)

        agents[0].authenticated = false
        let signedOut = SetupSnapshot(
            agents: agents,
            gitInstalled: true,
            githubCLIInstalled: true,
            githubAuthenticated: true,
            fractalCLIInstalled: true,
            fractalSocietyAuthenticated: true,
            fractalSocietyAccount: "@builder"
        )
        XCTAssertFalse(signedOut.isReady)
    }

    func testSetupDoesNotAcceptInstalledButSignedOutGitHubCLI() {
        var agents = SetupReadiness.agentTemplates
        agents[2].installed = true
        agents[2].authenticated = true
        let snapshot = SetupSnapshot(
            agents: agents,
            gitInstalled: true,
            githubCLIInstalled: true,
            githubAuthenticated: false,
            fractalCLIInstalled: true,
            fractalSocietyAuthenticated: true,
            fractalSocietyAccount: "@builder"
        )
        XCTAssertFalse(snapshot.isReady)
    }

    func testSetupRequiresFractalSocietyAuthentication() {
        var agents = SetupReadiness.agentTemplates
        agents[0].installed = true
        agents[0].authenticated = true
        let snapshot = SetupSnapshot(
            agents: agents,
            gitInstalled: true,
            githubCLIInstalled: true,
            githubAuthenticated: true,
            fractalCLIInstalled: true,
            fractalSocietyAuthenticated: false,
            fractalSocietyAccount: nil
        )
        XCTAssertFalse(snapshot.isReady)
    }

    func testFractalSocietyUsernameIsReadFromLoginStatus() {
        XCTAssertEqual(
            SetupReadiness.societyAccount(
                from: "Signed in to Fractal Society as @james-star."
            ),
            "@james-star"
        )
        XCTAssertNil(
            SetupReadiness.societyAccount(
                from: "Signed in to Fractal Society."
            )
        )
    }

    func testFractalSocietyVerificationMessageDoesNotClaimConnectionEarly() {
        XCTAssertEqual(
            SetupReadiness.verifyingSocietyMessage,
            "Authorization complete. Verifying your account…"
        )
        XCTAssertFalse(
            SetupReadiness.verifyingSocietyMessage.lowercased().contains("connected")
        )
    }

    func testVoiceEngineConfigurationLeavesRoomForLocalAndAPIProviders() {
        let configuration = VoiceEngineConfiguration()
        XCTAssertEqual(configuration.schema, "fractal.voice_engine.v1")
        XCTAssertEqual(configuration.transcriptionProvider, "granite-local")
        XCTAssertEqual(configuration.speechProvider, "kokoro-local")
        XCTAssertNil(configuration.customTranscriptionModel)
        XCTAssertNil(configuration.customSpeechModel)
        XCTAssertNil(configuration.apiProvider)
    }

    func testExternalVoiceChoicesDoNotRequireLocalModelDownload() {
        XCTAssertFalse(VoiceInputMode.chatGPTDesktop.requiresLocalModels)
        XCTAssertFalse(VoiceInputMode.superwhisper.requiresLocalModels)
        XCTAssertTrue(VoiceInputMode.chatGPTDesktop.isReady(localModelsReady: false))
        XCTAssertTrue(VoiceInputMode.superwhisper.isReady(localModelsReady: false))
    }

    func testChatGPTOnboardingUsesOfficialLinksAndIncludesVoiceIcon() {
        XCTAssertEqual(
            ChatGPTOnboarding.downloadURL.absoluteString,
            "https://chatgpt.com/download/"
        )
        XCTAssertEqual(
            ChatGPTOnboarding.permissionsURL.host,
            "learn.chatgpt.com"
        )
        XCTAssertNotNil(ChatGPTOnboarding.voiceIconURL)
    }

    func testBuiltInVoiceRequiresDownloadedModels() {
        XCTAssertTrue(VoiceInputMode.builtIn.requiresLocalModels)
        XCTAssertFalse(VoiceInputMode.builtIn.isReady(localModelsReady: false))
        XCTAssertTrue(VoiceInputMode.builtIn.isReady(localModelsReady: true))
    }

    func testVoiceInputChoicePersistsWithoutStartingDownloads() throws {
        let suite = "FractalVoiceTests.InputMode.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        XCTAssertNil(VoiceInputMode.selected(in: defaults))
        VoiceInputMode.save(.chatGPTDesktop, in: defaults)
        XCTAssertEqual(VoiceInputMode.selected(in: defaults), .chatGPTDesktop)
    }

    func testDownloadedVoiceModelsLiveOutsideTheApplicationBundle() {
        let expectedRoot = AppRuntime.isAppStoreEdition
            ? "/Fractal Voice/Models"
            : "/.fractal/models"
        XCTAssertTrue(VoiceModelManager.modelRoot.path.hasSuffix(expectedRoot))
        XCTAssertFalse(
            VoiceModelManager.modelRoot.path.hasPrefix(Bundle.main.bundlePath)
        )
        XCTAssertTrue(
            VoiceModelManager.graniteDirectory.path.hasSuffix(
                "\(expectedRoot)/granite-speech-4.1-2b-q4"
            )
        )
        XCTAssertTrue(
            VoiceModelManager.kokoroDirectory.path.hasSuffix(
                "\(expectedRoot)/kokoro-82m-bf16"
            )
        )
    }

    func testAgentAuthenticationOutputParsersAreExplicit() {
        XCTAssertTrue(
            SetupReadiness.authenticationSucceeded(
                for: "codex",
                result: CommandResult(launched: true, exitCode: 0, output: "Logged in")
            )
        )
        XCTAssertTrue(
            SetupReadiness.authenticationSucceeded(
                for: "claude",
                result: CommandResult(
                    launched: true,
                    exitCode: 0,
                    output: #"{"loggedIn":true}"#
                )
            )
        )
        XCTAssertFalse(
            SetupReadiness.authenticationSucceeded(
                for: "codex",
                result: CommandResult(launched: true, exitCode: 1, output: "Not logged in")
            )
        )
    }

    func testProjectNameCollisionMarkerTriggersConversationalRename() {
        XCTAssertTrue(
            BuildCoordinator.projectNameWasTaken(
                "Error: FRACTAL_PROJECT_NAME_TAKEN:pocket-ledger"
            )
        )
        XCTAssertFalse(
            BuildCoordinator.projectNameWasTaken(
                "project sync conflict: remote graph changed"
            )
        )
    }

    func testPreparingStateExplainsNativeEngineStartup() {
        XCTAssertEqual(VoiceState.preparing.label, "Starting voice engine…")
    }

    func testPlanningHeartbeatBecomesTheLiveHudSummary() {
        let line = "  ⏳ [claude] is selecting the architecture and component boundaries (user request) · 30s"
        XCTAssertEqual(
            BuildCoordinator.activitySummary(for: line),
            "⏳ [claude] is selecting the architecture and component boundaries (user request) · 30s"
        )
    }

    func testTerminalFormattingIsRemovedFromTaskSummaries() {
        XCTAssertEqual(
            BuildCoordinator.activitySummary(for: "\u{001B}[32m  ✓ lead proposed 12 validated tasks\u{001B}[0m"),
            "✓ lead proposed 12 validated tasks"
        )
    }

    func testGraniteTranscriptCleanupRemovesRuntimeMarkersOnly() {
        let output = """
        llama_model_loader: loaded metadata
        Loaded media from '/tmp/voice.wav'

        > transcribe the speech. Keywords: Fractal CLI, Codex.
        Build Sources/M3.11.swift with Codex and Cursor.
        [ Prompt: 400.0 t/s | Generation: 120.0 t/s ]
        <|end_of_text|>
        Exiting...
        """

        XCTAssertEqual(
            BuildCoordinator.cleanGraniteTranscript(output),
            "Build Sources/M3.11.swift with Codex and Cursor."
        )
    }

    func testGraniteRejectsCommonSilenceHallucinations() {
        XCTAssertTrue(BuildCoordinator.isLikelyGraniteHallucination("Thanks for watching."))
        XCTAssertTrue(BuildCoordinator.isLikelyGraniteHallucination("THANK YOU FOR WATCHING!"))
        XCTAssertTrue(BuildCoordinator.isLikelyGraniteHallucination("[Music]"))
        XCTAssertTrue(BuildCoordinator.isLikelyGraniteHallucination("All right."))
        XCTAssertFalse(
            BuildCoordinator.isLikelyGraniteHallucination(
                "Build a video player with playback controls."
            )
        )
    }

    func testWarmGraniteArgumentsReuseServerAndBoundShortAnswers() {
        let arguments = BuildCoordinator.graniteTranscriptionArguments(
            audioURL: URL(fileURLWithPath: "/tmp/yes.wav"),
            prompt: "Return yes or no.",
            assets: (
                URL(fileURLWithPath: "/models/granite.gguf"),
                URL(fileURLWithPath: "/models/mmproj.gguf")
            ),
            serverBaseURL: URL(string: "http://127.0.0.1:18371"),
            shortAnswer: true
        )
        XCTAssertEqual(
            Array(arguments.prefix(2)),
            ["--server-base", "http://127.0.0.1:18371"]
        )
        XCTAssertFalse(arguments.contains("--model"))
        XCTAssertEqual(
            arguments[arguments.firstIndex(of: "--n-predict")! + 1],
            "16"
        )
    }

    func testVoiceConfirmationUnderstandsCommonYesAndNoAnswers() {
        XCTAssertEqual(BuildCoordinator.yesNoAnswer("Yes, that's correct."), true)
        XCTAssertEqual(BuildCoordinator.yesNoAnswer("Nope, try again."), false)
        XCTAssertNil(BuildCoordinator.yesNoAnswer("Call it something else"))
    }

    func testConfirmationQuestionsRepeatTheRequestAndName() {
        XCTAssertEqual(
            BuildCoordinator.requestQuestion("Build an expense tracker"),
            "You said, “Build an expense tracker”. Is that correct?"
        )
        XCTAssertEqual(
            BuildCoordinator.nameQuestion("Pocket Ledger"),
            "Okay, I will call it “Pocket Ledger”. Is that correct?"
        )
    }

    func testConfirmationRepeatsTheTranscriptWithoutRewritingIt() {
        XCTAssertEqual(
            BuildCoordinator.requestQuestion("Can you please build me a hello app?"),
            "You said, “Can you please build me a hello app?”. Is that correct?"
        )
        XCTAssertEqual(
            BuildCoordinator.requestQuestion("Create an iOS expense tracker"),
            "You said, “Create an iOS expense tracker”. Is that correct?"
        )
        XCTAssertEqual(
            BuildCoordinator.requestQuestion("I want to build a hello app"),
            "You said, “I want to build a hello app”. Is that correct?"
        )
        XCTAssertEqual(
            BuildCoordinator.requestQuestion(
                "I want to build, want to build a hello app"
            ),
            "You said, “I want to build, want to build a hello app”. Is that correct?"
        )
    }

    func testProjectNameCleanupIsBoundedAndRemovesDictationPunctuation() {
        XCTAssertEqual(
            BuildCoordinator.cleanProjectName(" “Pocket Ledger!” "),
            "Pocket Ledger"
        )
        XCTAssertLessThanOrEqual(
            BuildCoordinator.cleanProjectName(String(repeating: "a", count: 200)).count,
            80
        )
    }

    func testManualProjectNamePreservesExactTypedPunctuation() {
        XCTAssertEqual(
            BuildCoordinator.cleanManualProjectName("  Pocket Ledger 2.0!  "),
            "Pocket Ledger 2.0!"
        )
    }

    @MainActor
    func testManualRequestHudSubmitsOnEnterPath() {
        var submitted = ""
        let hud = RecordingHUD(
            onStop: {},
            onRestart: {},
            onManualRequest: { submitted = $0 }
        )
        defer { hud.close() }

        hud.showManualRequest()
        hud.submitManualTextForTesting("Build a typed iOS app")

        XCTAssertTrue(hud.isShowingManualRequestForTesting)
        XCTAssertEqual(submitted, "Build a typed iOS app")
    }

    @MainActor
    func testFirstBuildUpdateEscapesGraniteVocabularyPhase() {
        let hud = RecordingHUD(onStop: {}, onRestart: {})
        defer { hud.close() }
        hud.showTranscribing()

        hud.updateBuilding(summary: "⏳ [codex] is selecting the architecture · 15s")

        XCTAssertTrue(hud.isShowingBuildProgressForTesting)
        XCTAssertEqual(
            hud.summaryForTesting,
            "⏳ [codex] is selecting the architecture · 15s"
        )
    }

    @MainActor
    func testConfirmationHudOffersTheQuestionPhase() {
        let hud = RecordingHUD(onStop: {}, onRestart: {}, onYes: {}, onNo: {})
        defer { hud.close() }

        hud.showQuestion("Is this what you want me to build?")

        XCTAssertTrue(hud.isShowingQuestionForTesting)
        XCTAssertEqual(
            hud.summaryForTesting,
            "Is this what you want me to build?"
        )
    }

    @MainActor
    func testVoiceHudCanBeMovedAndMinimizedToTheDock() {
        let hud = RecordingHUD(onStop: {}, onRestart: {})
        defer { hud.close() }

        XCTAssertTrue(hud.isMovableForTesting)
        XCTAssertTrue(hud.isMiniaturizableForTesting)
    }

    func testExternalDesktopHandoffIsPrivateFreshAndSingleUse() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fractal-handoff-\(UUID().uuidString).fractalbuild"
        )
        let payload: [String: Any] = [
            "schema": "fractal.external_build.v1",
            "request": "Build a very simple Hello World app.",
            "project_name": "Hello World",
            "created_at_ms": UInt64(Date().timeIntervalSince1970 * 1_000),
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        let handoff = try ExternalBuildHandoff.consume(url)

        XCTAssertEqual(handoff.request, "Build a very simple Hello World app.")
        XCTAssertEqual(handoff.projectName, "Hello World")
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testExternalDesktopHandoffRejectsExpiredRequests() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fractal-expired-\(UUID().uuidString).fractalbuild"
        )
        let payload: [String: Any] = [
            "schema": "fractal.external_build.v1",
            "request": "Build an expired app.",
            "project_name": "Expired",
            "created_at_ms": 1,
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        XCTAssertThrowsError(try ExternalBuildHandoff.consume(url))
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testExternalVisibilityHandoffIsPrivateFreshAndSingleUse() throws {
        let url = URL(fileURLWithPath: "/tmp").appendingPathComponent(
            "fractal-visibility-\(UUID().uuidString).fractalvisibility"
        )
        let payload: [String: Any] = [
            "schema": "fractal.external_visibility.v1",
            "workspace": "/Users/example/fractal-projects/racket",
            "target": "public",
            "created_at_ms": UInt64(Date().timeIntervalSince1970 * 1_000),
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        let handoff = try ExternalVisibilityHandoff.consume(url)

        XCTAssertEqual(handoff.workspace, "/Users/example/fractal-projects/racket")
        XCTAssertEqual(handoff.target, "public")
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testExternalXShareHandoffIsPrivateFreshAndRestrictedToXComposer() throws {
        let url = URL(fileURLWithPath: "/tmp").appendingPathComponent(
            "fractal-x-share-\(UUID().uuidString).fractalxshare"
        )
        let preview = "@helper @buildfractal Please help with task 2.4."
        var components = URLComponents(string: "https://x.com/intent/tweet")!
        components.queryItems = [URLQueryItem(name: "text", value: preview)]
        let payload: [String: Any] = [
            "schema": "fractal.external_x_share.v1",
            "intent_url": components.url!.absoluteString,
            "preview": preview,
            "created_at_ms": UInt64(Date().timeIntervalSince1970 * 1_000),
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        let handoff = try ExternalXShareHandoff.consume(url)

        XCTAssertEqual(handoff.preview, preview)
        XCTAssertEqual(handoff.intentURL.host, "x.com")
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testExternalXShareHandoffRejectsAnotherOrigin() throws {
        let url = URL(fileURLWithPath: "/tmp").appendingPathComponent(
            "fractal-x-share-\(UUID().uuidString).fractalxshare"
        )
        let payload: [String: Any] = [
            "schema": "fractal.external_x_share.v1",
            "intent_url": "https://evil.example/intent/tweet?text=Hello",
            "preview": "Hello",
            "created_at_ms": UInt64(Date().timeIntervalSince1970 * 1_000),
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        XCTAssertThrowsError(try ExternalXShareHandoff.consume(url))
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testExternalXShareHandoffAcceptsLegacyPlusEncodedSpaces() throws {
        let url = URL(fileURLWithPath: "/tmp").appendingPathComponent(
            "fractal-x-share-\(UUID().uuidString).fractalxshare"
        )
        let preview = "Building Coffee 2 with Fractal Society"
        let payload: [String: Any] = [
            "schema": "fractal.external_x_share.v1",
            "intent_url": "https://x.com/intent/tweet?text=Building+Coffee+2+with+Fractal+Society",
            "preview": preview,
            "created_at_ms": UInt64(Date().timeIntervalSince1970 * 1_000),
        ]
        XCTAssertTrue(FileManager.default.createFile(
            atPath: url.path,
            contents: try JSONSerialization.data(withJSONObject: payload),
            attributes: [.posixPermissions: 0o600]
        ))
        defer { try? FileManager.default.removeItem(at: url) }

        XCTAssertEqual(try ExternalXShareHandoff.consume(url).preview, preview)
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testWebsiteVisibilityHandoffAcceptsOnlyFractalSocietyCommands() throws {
        let handoff = try WebsiteVisibilityHandoff(url: URL(
            string: "fractalvoice://visibility?project=coffee5&target=private&server=https%3A%2F%2Ffractalsociety.com"
        )!)
        XCTAssertEqual(handoff.project, "coffee5")
        XCTAssertEqual(handoff.target, "private")
        XCTAssertThrowsError(try WebsiteVisibilityHandoff(url: URL(
            string: "fractalvoice://visibility?project=coffee5&target=private&server=https%3A%2F%2Fevil.example"
        )!))
    }

    func testExternalDesktopQueueDiscoversOnlyExpectedRegularFiles() throws {
        let directory = temporaryDirectory()
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        let expected = directory.appendingPathComponent(
            "fractal-build-123-abc.fractalbuild"
        )
        let unrelated = directory.appendingPathComponent(
            "other-build.fractalbuild"
        )
        let wrongExtension = directory.appendingPathComponent(
            "fractal-build-123-abc.json"
        )
        for url in [expected, unrelated, wrongExtension] {
            XCTAssertTrue(FileManager.default.createFile(
                atPath: url.path,
                contents: Data("{}".utf8),
                attributes: [.posixPermissions: 0o600]
            ))
        }

        let discovered = ExternalBuildHandoff.pendingURLs(in: directory)
        XCTAssertEqual(discovered.count, 1)
        XCTAssertEqual(discovered.first?.lastPathComponent, expected.lastPathComponent)
    }

    func testBuiltInVocabularyCorrectsFractalProductTerms() {
        let home = temporaryDirectory()
        let result = VoiceVocabularyEngine(homeURL: home).normalize(
            "Use fracture cli to make an execute asian graph with code x and cursor."
        )

        XCTAssertEqual(
            result.transcript,
            "Use Fractal CLI to make an execution graph with Codex and Cursor."
        )
        XCTAssertEqual(result.appliedCorrections.count, 5)
    }

    func testPersonalVocabularyOverridesBuiltInAndPreservesIdentifiers() throws {
        let home = temporaryDirectory()
        let engine = VoiceVocabularyEngine(homeURL: home)
        try FileManager.default.createDirectory(
            at: engine.personalURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let vocabulary = VoiceVocabularyFile(
            terms: ["AcmeGraph"],
            corrections: [
                "fracture cli": "Acme Fractal CLI",
                "acme graph": "AcmeGraph"
            ]
        )
        try JSONEncoder().encode(vocabulary).write(to: engine.personalURL)

        let result = engine.normalize(
            "Ask fracture cli to update acme graph at Sources/M3.11.swift."
        )

        XCTAssertEqual(
            result.transcript,
            "Ask Acme Fractal CLI to update AcmeGraph at Sources/M3.11.swift."
        )
    }

    func testProjectVocabularyTakesPrecedenceOverPersonalVocabulary() throws {
        let root = temporaryDirectory()
        let home = root.appendingPathComponent("home", isDirectory: true)
        let project = root.appendingPathComponent("project", isDirectory: true)
        let engine = VoiceVocabularyEngine(homeURL: home)
        try writeVocabulary(
            VoiceVocabularyFile(corrections: ["star forge": "Personal Forge"]),
            to: engine.personalURL
        )
        try writeVocabulary(
            VoiceVocabularyFile(
                terms: ["StarForge"],
                corrections: ["star forge": "StarForge"]
            ),
            to: project.appendingPathComponent(VoiceVocabularyEngine.projectRelativePath)
        )

        let result = engine.normalize("Build star forge.", projectURL: project)

        XCTAssertEqual(result.transcript, "Build StarForge.")
        XCTAssertTrue(engine.promptTerms(projectURL: project).contains("StarForge"))
    }

    func testPersonalVocabularyTemplateIsCreatedWithoutOverwritingIt() throws {
        let home = temporaryDirectory()
        let engine = VoiceVocabularyEngine(homeURL: home)

        try engine.installPersonalTemplateIfNeeded()
        let original = try Data(contentsOf: engine.personalURL)
        try engine.installPersonalTemplateIfNeeded()

        XCTAssertEqual(try Data(contentsOf: engine.personalURL), original)
        let decoded = try JSONDecoder().decode(
            VoiceVocabularyFile.self,
            from: original
        )
        XCTAssertEqual(decoded.schema, VoiceVocabularyFile.schema)
    }

    func testLegacyOnboardingFlagDoesNotSkipCurrentOnboarding() throws {
        let suiteName = "FractalVoiceTests.Onboarding.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        defaults.set(true, forKey: "completedOnboarding")

        XCTAssertFalse(OnboardingProgress.isComplete(defaults: defaults))
    }

    func testCurrentOnboardingCompletionIsRemembered() throws {
        let suiteName = "FractalVoiceTests.Onboarding.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        OnboardingProgress.markComplete(defaults: defaults)

        XCTAssertTrue(OnboardingProgress.isComplete(defaults: defaults))
        XCTAssertEqual(
            defaults.integer(forKey: OnboardingProgress.schemaVersionKey),
            OnboardingProgress.currentSchemaVersion
        )
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: url)
        }
        return url
    }

    private func writeVocabulary(
        _ vocabulary: VoiceVocabularyFile,
        to url: URL
    ) throws {
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try JSONEncoder().encode(vocabulary).write(to: url)
    }
}
