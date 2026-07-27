import Foundation

enum AppRuntime {
    #if APP_STORE
    static let isAppStoreEdition = true
    #else
    static let isAppStoreEdition = false
    #endif

    static var applicationSupportURL: URL {
        let base = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).first!
        return base.appendingPathComponent("Fractal Voice", isDirectory: true)
    }

    static var homeURL: URL {
        isAppStoreEdition ? applicationSupportURL : FileManager.default.homeDirectoryForCurrentUser
    }

    static var modelRoot: URL {
        if isAppStoreEdition {
            return applicationSupportURL.appendingPathComponent("Models", isDirectory: true)
        }
        return homeURL.appendingPathComponent(".fractal/models", isDirectory: true)
    }

    static var projectsURL: URL {
        if isAppStoreEdition {
            return applicationSupportURL.appendingPathComponent("Projects", isDirectory: true)
        }
        return homeURL.appendingPathComponent("fractal-projects", isDirectory: true)
    }

    static var logURL: URL {
        if isAppStoreEdition {
            return applicationSupportURL
                .appendingPathComponent("Logs", isDirectory: true)
                .appendingPathComponent("FractalVoice.log")
        }
        return homeURL.appendingPathComponent("Library/Logs/FractalVoice.log")
    }

    // Keep the sandbox test/App Store speech server separate from the
    // Developer ID app so both editions cannot fight over one loopback port.
    static var graniteServerPort: Int {
        isAppStoreEdition ? 18_374 : 18_371
    }
}
