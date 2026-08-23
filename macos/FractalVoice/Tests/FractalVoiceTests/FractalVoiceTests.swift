import Carbon.HIToolbox
import Foundation
import XCTest
@testable import FractalVoice

final class FractalVoiceTests: XCTestCase {
    private var packageRoot: URL {
        // `#filePath` points at Tests/FractalVoiceTests/FractalVoiceTests.swift.
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
    }

    func testAppMetadataDoesNotRequestMediaLibraryAccess() throws {
        let metadataURLs = [
            packageRoot.appendingPathComponent("Info.plist"),
            packageRoot.appendingPathComponent("AppStore/FractalVoice.entitlements"),
            packageRoot.appendingPathComponent("AppStore/FractalVoiceChild.entitlements"),
            packageRoot.appendingPathComponent("DeveloperID.entitlements"),
        ]
        let forbiddenKeys = [
            "NSAppleMusicUsageDescription",
            "NSMediaLibraryUsageDescription",
            "com.apple.security.media-library",
            "com.apple.security.media-library.read-write",
        ]

        for url in metadataURLs {
            let data = try Data(contentsOf: url)
            let propertyList = try XCTUnwrap(
                PropertyListSerialization.propertyList(
                    from: data,
                    options: [],
                    format: nil
                ) as? [String: Any]
            )
            for key in forbiddenKeys {
                XCTAssertNil(
                    propertyList[key],
                    "\(url.lastPathComponent) must not declare media-library access (\(key))"
                )
            }
        }
    }

    func testMacOSSourcesAvoidMediaLibraryAndNamedSoundLookups() throws {
        let sourceRoot = packageRoot.appendingPathComponent("Sources/FractalVoice")
        let forbiddenFragments = [
            "import MediaPlayer",
            "import MusicKit",
            "MPMediaLibrary",
            "MPNowPlaying",
            "SKCloudServiceController",
            "ITLibrary",
            "NSSound(named:",
            "UNNotificationSound",
            "requestAuthorization(options: [.alert, .sound])",
        ]
        let enumerator = try XCTUnwrap(
            FileManager.default.enumerator(
                at: sourceRoot,
                includingPropertiesForKeys: nil,
                options: [.skipsHiddenFiles]
            )
        )
        for case let url as URL in enumerator where url.pathExtension == "swift" {
            let source = try String(contentsOf: url, encoding: .utf8)
            for fragment in forbiddenFragments {
                XCTAssertFalse(
                    source.contains(fragment),
                    "\(url.lastPathComponent) must not reference \(fragment)"
                )
            }
        }
    }

    func testStartupAndExternalTextHandoffNeverRequestPermissions() {
        XCTAssertFalse(PermissionPolicy.shouldRequestMicrophone(in: .appLaunch))
        XCTAssertFalse(PermissionPolicy.shouldRequestNotifications(in: .appLaunch))
        XCTAssertFalse(PermissionPolicy.shouldRequestMicrophone(in: .externalTextHandoff))
        XCTAssertFalse(PermissionPolicy.shouldRequestNotifications(in: .externalTextHandoff))
    }

    func testPermissionRequestsRequireExplicitFeatureAction() {
        XCTAssertTrue(PermissionPolicy.shouldRequestMicrophone(in: .explicitVoiceRecording))
        XCTAssertTrue(PermissionPolicy.shouldRequestNotifications(in: .explicitNotificationOptIn))
        XCTAssertFalse(PermissionPolicy.shouldRequestNotifications(in: .backgroundBuildStatus))
    }

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

    func testVoiceBuildsUseNativeTextIngestWithoutBridgeRouting() throws {
        let arguments = BuildCoordinator.nativeTextIngestArguments(projectName: "Hello World")
        XCTAssertEqual(
            Array(arguments.prefix(10)),
            [
                "ingest",
                "--source", "fractal-mac-app",
                "--format", "text",
                "--stdin",
                "--managed-project",
                "--project-name", "Hello World",
            ]
        )
        XCTAssertFalse(arguments.contains("bridge"))

        let sourceRoot = packageRoot.appendingPathComponent("Sources/FractalVoice")
        for filename in ["OnboardingView.swift", "SetupReadiness.swift", "BuildCoordinator.swift"] {
            let source = try String(
                contentsOf: sourceRoot.appendingPathComponent(filename),
                encoding: .utf8
            )
            XCTAssertFalse(source.contains("fractal bridge"), "(filename) exposes the retired bridge")
            XCTAssertFalse(source.contains("LocalBridge"), "(filename) routes through LocalBridge")
            XCTAssertFalse(source.contains("pairing token"), "(filename) asks for a bridge token")
        }
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
        XCTAssertTrue(ChatGPTOnboarding.voiceLimitTip.contains("Superwhisper"))
        XCTAssertTrue(ChatGPTOnboarding.voiceLimitTip.contains("local voice models"))
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

    func testExternalBuildResultChannelIsPrivateAndStructured() throws {
        let requestURL = URL(fileURLWithPath: "/tmp").appendingPathComponent(
            "fractal-build-result-\(UUID().uuidString).fractalbuild"
        )
        let resultURL = ExternalBuildHandoff.resultURL(for: requestURL)
        defer { try? FileManager.default.removeItem(at: resultURL) }

        ExternalBuildHandoff.writeResult(
            to: resultURL,
            status: .projectNameTaken,
            projectName: "Hello World",
            message: "Project name is already taken. Retry with a different project name."
        )

        let attributes = try FileManager.default.attributesOfItem(atPath: resultURL.path)
        let permissions = (attributes[.posixPermissions] as? NSNumber)?.intValue ?? 0
        XCTAssertEqual(permissions & 0o777, 0o600)
        let payload = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: resultURL)) as? [String: Any]
        )
        XCTAssertEqual(payload["schema"] as? String, "fractal.external_build_result.v1")
        XCTAssertEqual(payload["status"] as? String, "project_name_taken")
        XCTAssertEqual(payload["project_name"] as? String, "Hello World")
    }

    func testExternalBuildResultRejectsPathsOutsideTemporaryDirectory() {
        let unsafeURL = URL(fileURLWithPath: "/tmp/../fractal-build-unsafe.result")
        try? FileManager.default.removeItem(at: unsafeURL)
        ExternalBuildHandoff.writeResult(
            to: unsafeURL,
            status: .failed,
            projectName: "Unsafe",
            message: "must not be written"
        )
        XCTAssertFalse(FileManager.default.fileExists(atPath: unsafeURL.path))
    }

    func testTextProjectActivityDoesNotUseVoiceLabel() {
        XCTAssertEqual(
            BuildCoordinator.activitySummary(for: "Created managed text project: /tmp/demo"),
            "Text project created — lead agent is preparing the plan…"
        )
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

    func testProjectFractalLearningFixturesDecodeFormatAndPresentWithoutCorruption() throws {
        let root = temporaryDirectory()
        let legacyURL = try writeProjectFractal(Self.legacyProjectFractalJSON, under: root, name: "legacy")
        let enrichedURL = try writeProjectFractal(Self.enrichedProjectFractalJSON, under: root, name: "enriched")
        let partialURL = try writeProjectFractal(Self.partialProjectFractalJSON, under: root, name: "partial")
        let futureURL = try writeProjectFractal(Self.futureLabelsProjectFractalJSON, under: root, name: "future")

        // Legacy: missing additive learning must not crash or look corrupted.
        let legacyEnvelope = try JSONDecoder().decode(
            ProjectGraphLearning.Envelope.self,
            from: Data(contentsOf: legacyURL)
        )
        XCTAssertEqual(legacyEnvelope.schema, "fractal.project.v1")
        XCTAssertNil(legacyEnvelope.learning)
        let legacyData = try Data(contentsOf: legacyURL)
        XCTAssertNil(ProjectGraphLearning.decode(from: legacyData))
        XCTAssertNil(BuildCoordinator.learningStatusPresentation(from: legacyURL))
        XCTAssertNil(BuildCoordinator.learningStatusPresentation(data: legacyData))

        // Fully enriched: Codable round-trip, formatting, graph edits, outcome.
        let enrichedData = try Data(contentsOf: enrichedURL)
        let enriched = try XCTUnwrap(ProjectGraphLearning.decode(from: enrichedData))
        XCTAssertEqual(enriched.schema, ProjectGraphLearning.schemaID)
        let node = try XCTUnwrap(enriched.nodes["n_auth"])
        XCTAssertEqual(node.outcome, "verified_success")
        XCTAssertEqual(node.failureCode, nil)
        XCTAssertEqual(node.attemptCount, 2)
        XCTAssertEqual(node.verification?.passed, true)
        XCTAssertEqual(node.artifactsProduced, ["artifact:commit-42"])
        XCTAssertTrue(node.humanIntervention)
        XCTAssertEqual(node.executor?.agent, "codex")
        XCTAssertEqual(node.outcomeText, "verified success")
        XCTAssertEqual(node.attemptText, "2 attempts")
        XCTAssertEqual(node.verificationText, "verified")
        XCTAssertEqual(
            node.compactSummary,
            "verified success · 2 attempts · verified · human assisted"
        )
        XCTAssertEqual(enriched.graphEdits.count, 1)
        XCTAssertEqual(
            enriched.graphEdits[0].compactSummary,
            "split · 2 nodes"
        )
        XCTAssertEqual(
            enriched.outcome?.compactSummary,
            "verified success · 100% verified · 1 retries · 1 human intervention"
        )
        XCTAssertEqual(
            enriched.compactCompletionSummary,
            "verified success · 100% verified · 1 retries · 1 human intervention"
        )

        let enrichedPresentation = try XCTUnwrap(
            BuildCoordinator.learningStatusPresentation(from: enrichedURL)
        )
        XCTAssertEqual(enrichedPresentation.nodeOutcome, "verified success")
        XCTAssertEqual(enrichedPresentation.nodeAttempt, "2 attempts")
        XCTAssertEqual(enrichedPresentation.nodeVerification, "verified")
        XCTAssertEqual(
            enrichedPresentation.graphSummary,
            "verified success · 100% verified · 1 retries · 1 human intervention"
        )
        let enrichedDetail = try XCTUnwrap(enrichedPresentation.detailLine)
        XCTAssertFalse(enrichedDetail.localizedCaseInsensitiveContains("corrupt"))
        XCTAssertFalse(enrichedDetail.localizedCaseInsensitiveContains("invalid"))

        // Partial: absent fields fall back safely; available fields still format.
        let partial = try XCTUnwrap(ProjectGraphLearning.load(from: partialURL))
        XCTAssertEqual(partial.nodes.count, 1)
        XCTAssertNil(partial.outcome)
        XCTAssertTrue(partial.graphEdits.isEmpty)
        let partialNode = try XCTUnwrap(partial.nodes["n_partial"])
        XCTAssertNil(partialNode.outcome)
        XCTAssertEqual(partialNode.attemptCount, 1)
        XCTAssertNil(partialNode.verification)
        XCTAssertEqual(partialNode.outcomeText, "not finished")
        XCTAssertEqual(partialNode.attemptText, "1 attempt")
        XCTAssertEqual(partialNode.verificationText, "not verified")
        XCTAssertEqual(partialNode.compactSummary, "not finished · 1 attempt")
        XCTAssertEqual(partial.compactCompletionSummary, "0/1 verified · 1 attempt")

        let partialPresentation = try XCTUnwrap(
            BuildCoordinator.learningStatusPresentation(from: partialURL)
        )
        XCTAssertEqual(partialPresentation.nodeOutcome, "not finished")
        XCTAssertEqual(partialPresentation.nodeAttempt, "1 attempt")
        XCTAssertEqual(partialPresentation.nodeVerification, "not verified")
        // Missing additive outcome must not surface as corruption.
        let partialDetail = try XCTUnwrap(partialPresentation.detailLine)
        XCTAssertFalse(partialDetail.localizedCaseInsensitiveContains("corrupt"))
        XCTAssertFalse(partialDetail.localizedCaseInsensitiveContains("invalid"))
        XCTAssertFalse(partialDetail.contains("not verified"))

        // Empty learning shell: no displayable fields → nil presentation, not failure.
        let emptyLearning = """
        {"schema":"fractal.project.v1","learning":{"schema":"fractal.learning.v1","nodes":{},"graph_edits":[]}}
        """.data(using: .utf8)!
        XCTAssertNotNil(ProjectGraphLearning.decode(from: emptyLearning))
        XCTAssertNil(BuildCoordinator.learningStatusPresentation(data: emptyLearning))

        // Unknown future controlled strings remain readable and re-encodable.
        let future = try XCTUnwrap(ProjectGraphLearning.load(from: futureURL))
        let futureNode = try XCTUnwrap(future.nodes["n_7"])
        XCTAssertEqual(futureNode.outcome, "future_success_kind")
        XCTAssertEqual(futureNode.failureCode, "future_failure_code")
        XCTAssertNil(futureNode.knownOutcome)
        XCTAssertNil(futureNode.knownFailureCode)
        XCTAssertEqual(
            futureNode.outcomeText,
            "future success kind (future failure code)"
        )
        XCTAssertEqual(
            futureNode.compactSummary,
            "future success kind (future failure code) · 2 attempts · verified"
        )
        XCTAssertEqual(future.graphEdits.first?.kind, "future_split_kind")
        XCTAssertEqual(
            future.graphEdits.first?.compactSummary,
            "future split kind · 1 node"
        )
        XCTAssertEqual(
            future.outcome?.compactSummary,
            "verified success · 100% verified · 1 retries"
        )
        let reencoded = try JSONEncoder().encode(future)
        let roundTripped = try XCTUnwrap(ProjectGraphLearning.decode(from: """
        {"learning":\(String(data: reencoded, encoding: .utf8)!)}
        """.data(using: .utf8)!))
        XCTAssertEqual(roundTripped.nodes["n_7"]?.outcome, "future_success_kind")
        XCTAssertEqual(roundTripped.graphEdits.first?.kind, "future_split_kind")

        let futurePresentation = try XCTUnwrap(
            BuildCoordinator.learningStatusPresentation(from: futureURL)
        )
        XCTAssertTrue(
            (futurePresentation.nodeOutcome ?? "")
                .contains("future success kind")
        )
        XCTAssertFalse(
            (futurePresentation.detailLine ?? "")
                .localizedCaseInsensitiveContains("corrupt")
        )
    }

    @MainActor
    func testLearningStatusPresentationFeedsHUDWithoutStartingABuild() throws {
        let root = temporaryDirectory()
        let enrichedURL = try writeProjectFractal(
            Self.enrichedProjectFractalJSON,
            under: root,
            name: "hud-enriched"
        )
        let legacyURL = try writeProjectFractal(
            Self.legacyProjectFractalJSON,
            under: root,
            name: "hud-legacy"
        )

        let presentation = try XCTUnwrap(
            BuildCoordinator.learningStatusPresentation(from: enrichedURL)
        )
        let detail = try XCTUnwrap(presentation.detailLine)

        let hud = RecordingHUD(onStop: {}, onRestart: {})
        defer { hud.close() }

        hud.showBuilding(summary: "Building authentication…", detail: detail)
        XCTAssertTrue(hud.isShowingBuildProgressForTesting)
        XCTAssertEqual(hud.summaryForTesting, "Building authentication…")
        XCTAssertEqual(hud.detailForTesting, detail)
        XCTAssertFalse((hud.detailForTesting ?? "").localizedCaseInsensitiveContains("corrupt"))

        hud.updateLearningDetail(
            BuildCoordinator.learningStatusPresentation(from: legacyURL)?.detailLine
        )
        XCTAssertNil(hud.detailForTesting)

        hud.showBuilding(summary: "Still building…")
        XCTAssertNil(hud.detailForTesting)
        hud.updateBuilding(
            summary: "Still building…",
            detail: presentation.finishedSummary,
            updateDetail: true
        )
        XCTAssertEqual(hud.detailForTesting, presentation.finishedSummary)
    }

    private static let legacyProjectFractalJSON = """
    {
      "schema": "fractal.project.v1",
      "graph": {
        "schema": "fractal.execution_graph.v1",
        "nodes": [{"id": "n1", "title": "Legacy task"}],
        "edges": []
      }
    }
    """

    private static let enrichedProjectFractalJSON = """
    {
      "schema": "fractal.project.v1",
      "learning": {
        "schema": "fractal.learning.v1",
        "nodes": {
          "n_auth": {
            "node_id": "n_auth",
            "node_type": "implementation",
            "objective": "Implement authentication endpoint",
            "title": "Auth endpoint",
            "depends_on": ["n_contract"],
            "executor": {
              "agent": "codex",
              "model": "gpt-5",
              "version": "1.0.0",
              "label": "worker-1"
            },
            "attempt_count": 2,
            "outcome": "verified_success",
            "verification": {
              "type": "integration_test",
              "status": "passed",
              "passed": true,
              "evidence_refs": ["artifact:test-result-17"]
            },
            "artifacts_produced": ["artifact:commit-42"],
            "artifact_refs": [{"ref": "artifact:commit-42", "kind": "commit"}],
            "consumed_by": ["n_verify"],
            "human_intervention": true,
            "intervention": {
              "required": true,
              "actor": "owner",
              "reason": "approved split"
            },
            "started_at": "2026-08-02T12:00:00Z",
            "completed_at": "2026-08-02T12:05:00Z"
          }
        },
        "graph_edits": [
          {
            "event_id": "ge_1",
            "kind": "split",
            "actor": "owner",
            "reason": "node too broad",
            "affected_node_ids": ["n_auth"],
            "created_node_ids": ["n_verify"],
            "removed_node_ids": [],
            "occurred_at": "2026-08-02T11:59:00Z",
            "artifact_refs": [{"ref": "artifact:edit-1"}]
          }
        ],
        "artifact_refs": [{"ref": "artifact:commit-42"}],
        "outcome": {
          "final_verified_success": true,
          "total_cost": 0.11,
          "retry_count": 1,
          "reopened_node_count": 0,
          "human_intervention_count": 1,
          "verification_coverage": 1.0,
          "completed_node_count": 1,
          "failed_node_count": 0,
          "blocked_node_count": 0,
          "artifact_count": 1
        }
      }
    }
    """

    private static let partialProjectFractalJSON = """
    {
      "schema": "fractal.project.v1",
      "learning": {
        "schema": "fractal.learning.v1",
        "nodes": {
          "n_partial": {
            "node_id": "n_partial",
            "node_type": "implementation",
            "objective": "Draft interface only",
            "attempt_count": 1,
            "started_at": "2026-08-02T12:01:00Z"
          }
        }
      }
    }
    """

    private static let futureLabelsProjectFractalJSON = """
    {
      "schema": "fractal.project.v1",
      "learning": {
        "schema": "fractal.learning.v1",
        "nodes": {
          "n_7": {
            "node_id": "n_7",
            "node_type": "implementation",
            "objective": "Implement authentication endpoint",
            "depends_on": ["n_2"],
            "attempt_count": 2,
            "outcome": "future_success_kind",
            "failure_code": "future_failure_code",
            "verification": {
              "type": "integration_test",
              "passed": true,
              "evidence_refs": ["artifact:test-result-17"]
            },
            "artifacts_produced": ["artifact:commit-42"],
            "consumed_by": ["n_9"],
            "human_intervention": false,
            "completed_at": "2026-08-02T12:10:00Z"
          }
        },
        "graph_edits": [
          {
            "event_id": "ge_future",
            "kind": "future_split_kind",
            "affected_node_ids": ["n_7"],
            "created_node_ids": [],
            "removed_node_ids": []
          }
        ],
        "outcome": {
          "final_verified_success": true,
          "total_cost": 0.11,
          "retry_count": 1,
          "reopened_node_count": 0,
          "human_intervention_count": 0,
          "verification_coverage": 1.0
        }
      }
    }
    """

    private func writeProjectFractal(
        _ json: String,
        under root: URL,
        name: String
    ) throws -> URL {
        let project = root
            .appendingPathComponent(name, isDirectory: true)
            .appendingPathComponent(".fractal", isDirectory: true)
        try FileManager.default.createDirectory(
            at: project,
            withIntermediateDirectories: true
        )
        let url = project.appendingPathComponent("project.fractal")
        try Data(json.utf8).write(to: url)
        return url
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
