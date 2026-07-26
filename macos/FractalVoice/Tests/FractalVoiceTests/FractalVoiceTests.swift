import XCTest
@testable import FractalVoice

final class FractalVoiceTests: XCTestCase {
    func testShortcutIsVisibleAndStable() {
        XCTAssertEqual(GlobalHotKey.displayName, "⌃⌥Space")
    }

    func testProcessPathIncludesAgentInstallLocations() {
        let path = BuildCoordinator.processEnvironment()["PATH"] ?? ""
        XCTAssertTrue(path.contains(".cargo/bin"))
        XCTAssertTrue(path.contains(".local/bin"))
        XCTAssertTrue(path.contains("/opt/homebrew/bin"))
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
}
