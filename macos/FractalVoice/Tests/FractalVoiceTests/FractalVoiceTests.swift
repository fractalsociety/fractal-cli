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
}
