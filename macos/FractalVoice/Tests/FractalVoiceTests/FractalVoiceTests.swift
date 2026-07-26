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
        XCTAssertEqual(
            detector.observe(rms: 0.03, duration: 0.08),
            .speechStarted
        )
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
        XCTAssertEqual(
            detector.observe(rms: 0.02, duration: 0.08),
            .speechStarted
        )
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

    func testProcessPathIncludesAgentInstallLocations() {
        let path = BuildCoordinator.processEnvironment()["PATH"] ?? ""
        XCTAssertTrue(path.contains(".cargo/bin"))
        XCTAssertTrue(path.contains(".local/bin"))
        XCTAssertTrue(path.contains("/opt/homebrew/bin"))
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
