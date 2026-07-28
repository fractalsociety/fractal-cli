import Foundation

enum AppRuntime {
    static let projectsDirectoryPathKey = "projectsDirectoryPath"
    static let projectsDirectoryBookmarkKey = "projectsDirectoryBookmark"

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

    static var defaultProjectsURL: URL {
        if isAppStoreEdition {
            return applicationSupportURL.appendingPathComponent("Projects", isDirectory: true)
        }
        return homeURL.appendingPathComponent("fractal-projects", isDirectory: true)
    }

    static var projectsURL: URL {
        projectsURL(in: .standard)
    }

    static func projectsURL(in defaults: UserDefaults) -> URL {
        if isAppStoreEdition,
           let bookmark = defaults.data(forKey: projectsDirectoryBookmarkKey),
           let resolved = try? resolveProjectsBookmark(bookmark, defaults: defaults) {
            _ = resolved.startAccessingSecurityScopedResource()
            return resolved
        }
        guard
            let configured = defaults.string(forKey: projectsDirectoryPathKey),
            !configured.isEmpty
        else {
            return defaultProjectsURL
        }
        return URL(fileURLWithPath: configured, isDirectory: true).standardizedFileURL
    }

    static func configureProjectsURL(
        _ url: URL,
        defaults: UserDefaults = .standard,
        agentInstructions: String? = nil
    ) throws {
        let selected = url.standardizedFileURL
        guard selected.isFileURL, selected.path.hasPrefix("/") else {
            throw ProjectsDirectoryError.absolutePathRequired
        }
        try FileManager.default.createDirectory(
            at: selected,
            withIntermediateDirectories: true
        )
        if isAppStoreEdition {
            let bookmark = try selected.bookmarkData(
                options: .withSecurityScope,
                includingResourceValuesForKeys: nil,
                relativeTo: nil
            )
            defaults.set(bookmark, forKey: projectsDirectoryBookmarkKey)
        }
        defaults.set(selected.path, forKey: projectsDirectoryPathKey)
        if let agentInstructions {
            try installGlobalAgentInstructions(at: selected, contents: agentInstructions)
        } else {
            try prepareProjectsDirectory(at: selected)
        }
    }

    static func useDefaultProjectsURL(defaults: UserDefaults = .standard) throws {
        defaults.removeObject(forKey: projectsDirectoryPathKey)
        defaults.removeObject(forKey: projectsDirectoryBookmarkKey)
        try prepareProjectsDirectory(at: defaultProjectsURL)
    }

    static func prepareProjectsDirectory(at root: URL? = nil) throws {
        let root = (root ?? projectsURL).standardizedFileURL
        try FileManager.default.createDirectory(
            at: root,
            withIntermediateDirectories: true
        )
        let destination = root.appendingPathComponent("AGENTS.md")
        guard !FileManager.default.fileExists(atPath: destination.path) else {
            return
        }
        guard
            let template = Bundle.main.resourceURL?.appendingPathComponent("AGENTS.md"),
            FileManager.default.fileExists(atPath: template.path)
        else {
            throw ProjectsDirectoryError.agentInstructionsMissing
        }
        try FileManager.default.copyItem(at: template, to: destination)
    }

    static func installGlobalAgentInstructions(
        at root: URL,
        contents: String
    ) throws {
        try FileManager.default.createDirectory(
            at: root,
            withIntermediateDirectories: true
        )
        let destination = root.appendingPathComponent("AGENTS.md")
        if FileManager.default.fileExists(atPath: destination.path) {
            let current = try String(contentsOf: destination, encoding: .utf8)
            if current.hasPrefix("# Fractal Agent Operating Contract"),
               current != contents {
                try contents.write(
                    to: destination,
                    atomically: true,
                    encoding: .utf8
                )
            }
            return
        }
        try contents.write(to: destination, atomically: true, encoding: .utf8)
    }

    private static func resolveProjectsBookmark(
        _ bookmark: Data,
        defaults: UserDefaults
    ) throws -> URL {
        var stale = false
        let url = try URL(
            resolvingBookmarkData: bookmark,
            options: .withSecurityScope,
            relativeTo: nil,
            bookmarkDataIsStale: &stale
        )
        if stale {
            let refreshed = try url.bookmarkData(
                options: .withSecurityScope,
                includingResourceValuesForKeys: nil,
                relativeTo: nil
            )
            defaults.set(refreshed, forKey: projectsDirectoryBookmarkKey)
        }
        return url.standardizedFileURL
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

enum ProjectsDirectoryError: LocalizedError {
    case absolutePathRequired
    case agentInstructionsMissing

    var errorDescription: String? {
        switch self {
        case .absolutePathRequired:
            return "Choose an absolute local project folder."
        case .agentInstructionsMissing:
            return "The installed app is missing its AGENTS.md template. Reinstall Fractal Voice."
        }
    }
}
