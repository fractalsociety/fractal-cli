import Darwin
import Foundation

struct ExternalXShareRequest: Equatable {
    let intentURL: URL
    let preview: String
}

enum ExternalXShareHandoff {
    private static let schema = "fractal.external_x_share.v1"
    private static let prefix = "fractal-x-share-"
    private static let maximumAgeMilliseconds: UInt64 = 2 * 60 * 1_000

    private struct Envelope: Decodable {
        let schema: String
        let intentURL: String
        let preview: String
        let createdAtMilliseconds: UInt64

        enum CodingKeys: String, CodingKey {
            case schema, preview
            case intentURL = "intent_url"
            case createdAtMilliseconds = "created_at_ms"
        }
    }

    static func pendingURLs(
        in directory: URL = URL(fileURLWithPath: "/tmp", isDirectory: true)
    ) -> [URL] {
        let keys: Set<URLResourceKey> = [
            .isRegularFileKey,
            .isSymbolicLinkKey,
            .creationDateKey,
        ]
        return ((try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: Array(keys),
            options: [.skipsHiddenFiles]
        )) ?? []).filter { url in
            guard
                url.pathExtension == "fractalxshare",
                url.lastPathComponent.hasPrefix(prefix),
                let values = try? url.resourceValues(forKeys: keys)
            else {
                return false
            }
            return values.isRegularFile == true && values.isSymbolicLink != true
        }.sorted {
            ((try? $0.resourceValues(forKeys: [.creationDateKey]).creationDate) ?? .distantFuture)
                < ((try? $1.resourceValues(forKeys: [.creationDateKey]).creationDate)
                    ?? .distantFuture)
        }
    }

    static func consume(_ sourceURL: URL) throws -> ExternalXShareRequest {
        let fileManager = FileManager.default
        let url = sourceURL.standardizedFileURL
        let resolved = url.resolvingSymlinksInPath().standardizedFileURL
        guard
            url.pathExtension == "fractalxshare",
            resolved.path.hasPrefix("/private/tmp/") || resolved.path.hasPrefix("/tmp/")
        else {
            throw ExternalXShareError.invalidFile
        }
        let resources = try url.resourceValues(forKeys: [
            .isRegularFileKey,
            .isSymbolicLinkKey,
            .fileSizeKey,
        ])
        guard
            resources.isRegularFile == true,
            resources.isSymbolicLink != true,
            let size = resources.fileSize,
            size > 0,
            size <= 16 * 1024
        else {
            throw ExternalXShareError.invalidFile
        }
        let attributes = try fileManager.attributesOfItem(atPath: url.path)
        guard
            (attributes[.ownerAccountID] as? NSNumber)?.uint32Value == getuid(),
            ((attributes[.posixPermissions] as? NSNumber)?.intValue ?? -1) & 0o777 == 0o600
        else {
            throw ExternalXShareError.insecurePermissions
        }

        defer { try? fileManager.removeItem(at: url) }
        let envelope = try JSONDecoder().decode(
            Envelope.self,
            from: Data(contentsOf: url, options: .mappedIfSafe)
        )
        let now = UInt64(Date().timeIntervalSince1970 * 1_000)
        guard
            envelope.schema == schema,
            envelope.createdAtMilliseconds <= now + 10_000,
            now >= envelope.createdAtMilliseconds,
            now - envelope.createdAtMilliseconds <= maximumAgeMilliseconds
        else {
            throw ExternalXShareError.invalidRequest
        }

        let preview = envelope.preview.trimmingCharacters(in: .whitespacesAndNewlines)
        guard
            !preview.isEmpty,
            preview.count <= 280,
            let intentURL = URL(string: envelope.intentURL),
            let components = URLComponents(url: intentURL, resolvingAgainstBaseURL: false),
            components.scheme == "https",
            components.host?.lowercased() == "x.com",
            components.path == "/intent/tweet",
            components.fragment == nil,
            components.queryItems?.count == 1,
            components.queryItems?.first?.name == "text",
            components.queryItems?.first?.value == preview
        else {
            throw ExternalXShareError.invalidRequest
        }
        return ExternalXShareRequest(intentURL: intentURL, preview: preview)
    }
}

enum ExternalXShareError: LocalizedError {
    case invalidFile
    case insecurePermissions
    case invalidRequest

    var errorDescription: String? {
        switch self {
        case .invalidFile:
            return "The external X share request is not a valid temporary handoff."
        case .insecurePermissions:
            return "The external X share request is not private to this macOS user."
        case .invalidRequest:
            return "The external X share request is invalid, expired, or not an X composer URL."
        }
    }
}
