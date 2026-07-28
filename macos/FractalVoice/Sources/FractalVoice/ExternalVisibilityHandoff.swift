import Darwin
import Foundation

struct ExternalVisibilityRequest: Equatable {
    let workspace: String
    let target: String
}

enum ExternalVisibilityHandoff {
    private static let schema = "fractal.external_visibility.v1"
    private static let prefix = "fractal-visibility-"
    private static let maximumAgeMilliseconds: UInt64 = 2 * 60 * 1_000

    private struct Envelope: Decodable {
        let schema: String
        let workspace: String
        let target: String
        let createdAtMilliseconds: UInt64

        enum CodingKeys: String, CodingKey {
            case schema, workspace, target
            case createdAtMilliseconds = "created_at_ms"
        }
    }

    static func pendingURLs() -> [URL] {
        let directory = URL(fileURLWithPath: "/tmp", isDirectory: true)
        return ((try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: [.isRegularFileKey, .isSymbolicLinkKey, .creationDateKey]
        )) ?? []).filter { url in
            url.pathExtension == "fractalvisibility"
                && url.lastPathComponent.hasPrefix(prefix)
        }.sorted {
            ((try? $0.resourceValues(forKeys: [.creationDateKey]).creationDate) ?? .distantFuture)
                < ((try? $1.resourceValues(forKeys: [.creationDateKey]).creationDate)
                    ?? .distantFuture)
        }
    }

    static func consume(_ sourceURL: URL) throws -> ExternalVisibilityRequest {
        let url = sourceURL.standardizedFileURL
        let resolved = url.resolvingSymlinksInPath().standardizedFileURL
        guard
            url.pathExtension == "fractalvisibility",
            resolved.path.hasPrefix("/private/tmp/") || resolved.path.hasPrefix("/tmp/")
        else {
            throw ExternalVisibilityError.invalidFile
        }
        let resources = try url.resourceValues(forKeys: [
            .isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey,
        ])
        guard
            resources.isRegularFile == true,
            resources.isSymbolicLink != true,
            let size = resources.fileSize,
            size > 0,
            size <= 16 * 1024
        else {
            throw ExternalVisibilityError.invalidFile
        }
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        guard
            (attributes[.ownerAccountID] as? NSNumber)?.uint32Value == getuid(),
            ((attributes[.posixPermissions] as? NSNumber)?.intValue ?? -1) & 0o777 == 0o600
        else {
            throw ExternalVisibilityError.insecurePermissions
        }
        defer { try? FileManager.default.removeItem(at: url) }
        let envelope = try JSONDecoder().decode(
            Envelope.self,
            from: Data(contentsOf: url, options: .mappedIfSafe)
        )
        let now = UInt64(Date().timeIntervalSince1970 * 1_000)
        guard
            envelope.schema == schema,
            envelope.createdAtMilliseconds <= now + 10_000,
            now >= envelope.createdAtMilliseconds,
            now - envelope.createdAtMilliseconds <= maximumAgeMilliseconds,
            ["public", "private"].contains(envelope.target),
            envelope.workspace.hasPrefix("/")
        else {
            throw ExternalVisibilityError.invalidRequest
        }
        return ExternalVisibilityRequest(
            workspace: envelope.workspace,
            target: envelope.target
        )
    }
}

enum ExternalVisibilityError: LocalizedError {
    case invalidFile
    case insecurePermissions
    case invalidRequest

    var errorDescription: String? {
        switch self {
        case .invalidFile:
            return "The external visibility request is not a valid temporary handoff."
        case .insecurePermissions:
            return "The external visibility request is not private to this macOS user."
        case .invalidRequest:
            return "The external visibility request is invalid or expired."
        }
    }
}
