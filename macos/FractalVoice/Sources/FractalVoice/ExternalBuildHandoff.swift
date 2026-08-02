import Darwin
import Foundation

struct ExternalBuildRequest: Equatable {
    let request: String
    let projectName: String
}

enum ExternalBuildResultStatus: String {
    case started
    case accepted
    case projectNameTaken = "project_name_taken"
    case failed
}

enum ExternalBuildHandoff {
    private static let queuedFilePrefix = "fractal-build-"
    private static let schema = "fractal.external_build.v1"
    private static let resultSchema = "fractal.external_build_result.v1"
    private static let maximumBytes = 40 * 1024
    private static let maximumAgeMilliseconds: UInt64 = 2 * 60 * 1_000

    private struct Envelope: Decodable {
        let schema: String
        let request: String
        let projectName: String
        let createdAtMilliseconds: UInt64

        enum CodingKeys: String, CodingKey {
            case schema
            case request
            case projectName = "project_name"
            case createdAtMilliseconds = "created_at_ms"
        }
    }

    static func consume(_ sourceURL: URL) throws -> ExternalBuildRequest {
        let fileManager = FileManager.default
        let url = sourceURL.standardizedFileURL
        let allowedTemporaryDirectories = [
            URL(fileURLWithPath: "/tmp", isDirectory: true),
            fileManager.temporaryDirectory,
        ].map {
            $0.resolvingSymlinksInPath().standardizedFileURL
        }
        let resolved = url.resolvingSymlinksInPath().standardizedFileURL
        guard
            url.pathExtension == "fractalbuild",
            allowedTemporaryDirectories.contains(where: {
                resolved.path.hasPrefix($0.path + "/")
            })
        else {
            throw ExternalBuildHandoffError.outsideTemporaryDirectory
        }

        let resources = try url.resourceValues(forKeys: [
            .isRegularFileKey,
            .isSymbolicLinkKey,
            .fileSizeKey,
        ])
        guard resources.isRegularFile == true, resources.isSymbolicLink != true else {
            throw ExternalBuildHandoffError.invalidFile
        }
        guard let fileSize = resources.fileSize, fileSize > 0, fileSize <= maximumBytes else {
            throw ExternalBuildHandoffError.invalidSize
        }
        let attributes = try fileManager.attributesOfItem(atPath: url.path)
        let ownerID = (attributes[.ownerAccountID] as? NSNumber)?.uint32Value
        let permissions = (attributes[.posixPermissions] as? NSNumber)?.intValue
        guard
            ownerID == getuid(),
            permissions.map({ $0 & 0o777 }) == 0o600
        else {
            throw ExternalBuildHandoffError.insecurePermissions
        }

        defer { try? fileManager.removeItem(at: url) }
        let envelope = try JSONDecoder().decode(
            Envelope.self,
            from: Data(contentsOf: url, options: .mappedIfSafe)
        )
        guard envelope.schema == schema else {
            throw ExternalBuildHandoffError.invalidSchema
        }
        let now = UInt64(Date().timeIntervalSince1970 * 1_000)
        guard
            envelope.createdAtMilliseconds <= now + 10_000,
            now.saturatingSubtract(envelope.createdAtMilliseconds) <= maximumAgeMilliseconds
        else {
            throw ExternalBuildHandoffError.expired
        }

        let request = envelope.request.trimmingCharacters(in: .whitespacesAndNewlines)
        let projectName = envelope.projectName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !request.isEmpty, request.utf8.count <= 32 * 1024 else {
            throw ExternalBuildHandoffError.invalidRequest
        }
        guard
            !projectName.isEmpty,
            projectName.count <= 80,
            !projectName.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else {
            throw ExternalBuildHandoffError.invalidProjectName
        }
        return ExternalBuildRequest(request: request, projectName: projectName)
    }

    static func pendingURLs(
        in directory: URL = URL(fileURLWithPath: "/tmp", isDirectory: true)
    ) -> [URL] {
        let keys: Set<URLResourceKey> = [
            .isRegularFileKey,
            .isSymbolicLinkKey,
            .creationDateKey,
        ]
        let urls = (try? FileManager.default.contentsOfDirectory(
            at: directory,
            includingPropertiesForKeys: Array(keys),
            options: [.skipsHiddenFiles]
        )) ?? []
        return urls.filter { url in
            guard
                url.pathExtension == "fractalbuild",
                url.lastPathComponent.hasPrefix(queuedFilePrefix),
                let values = try? url.resourceValues(forKeys: keys)
            else {
                return false
            }
            return values.isRegularFile == true && values.isSymbolicLink != true
        }.sorted { lhs, rhs in
            let left = (try? lhs.resourceValues(forKeys: [.creationDateKey]).creationDate)
                ?? .distantFuture
            let right = (try? rhs.resourceValues(forKeys: [.creationDateKey]).creationDate)
                ?? .distantFuture
            return left < right
        }
    }

    static func resultURL(for sourceURL: URL) -> URL {
        sourceURL.deletingPathExtension().appendingPathExtension("result")
    }

    /// Write an owner-only, atomically replaced result for the external text
    /// caller. The CLI treats `started` as non-terminal while it waits for the
    /// authoritative project-name check to complete.
    static func writeResult(
        to url: URL,
        status: ExternalBuildResultStatus,
        projectName: String,
        message: String
    ) {
        let fileManager = FileManager.default
        let resolvedParent = url.deletingLastPathComponent()
            .resolvingSymlinksInPath()
            .standardizedFileURL
        let allowedParents = [
            URL(fileURLWithPath: "/tmp", isDirectory: true),
            fileManager.temporaryDirectory,
        ].map { $0.resolvingSymlinksInPath().standardizedFileURL }
        guard
            url.pathExtension == "result",
            url.lastPathComponent.hasPrefix("fractal-build-"),
            allowedParents.contains(where: {
                resolvedParent.path == $0.path
                    || resolvedParent.path.hasPrefix($0.path + "/")
            })
        else {
            return
        }
        let payload: [String: Any] = [
            "schema": resultSchema,
            "status": status.rawValue,
            "project_name": projectName,
            "message": message,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else {
            return
        }
        let temporary = url
            .deletingLastPathComponent()
            .appendingPathComponent(
                ".\(url.lastPathComponent).\(UUID().uuidString).tmp"
            )
        guard fileManager.createFile(
            atPath: temporary.path,
            contents: data,
            attributes: [.posixPermissions: 0o600]
        ) else {
            return
        }
        // A rename is atomic on the same filesystem and avoids exposing a
        // partially written JSON document to the waiting CLI.
        guard rename(temporary.path, url.path) == 0 else {
            try? fileManager.removeItem(at: temporary)
            return
        }
        try? fileManager.setAttributes(
            [.posixPermissions: 0o600],
            ofItemAtPath: url.path
        )
        // A caller that timed out while the app was queued should not leave
        // a durable response in /tmp. Give the normal CLI reader ample time,
        // then remove terminal results automatically.
        DispatchQueue.global().asyncAfter(deadline: .now() + 120) {
            try? fileManager.removeItem(at: url)
        }
    }
}

private extension UInt64 {
    func saturatingSubtract(_ other: UInt64) -> UInt64 {
        self >= other ? self - other : 0
    }
}

enum ExternalBuildHandoffError: LocalizedError {
    case outsideTemporaryDirectory
    case invalidFile
    case invalidSize
    case insecurePermissions
    case invalidSchema
    case expired
    case invalidRequest
    case invalidProjectName

    var errorDescription: String? {
        switch self {
        case .outsideTemporaryDirectory:
            return "External build requests must come from the secure temporary handoff."
        case .invalidFile:
            return "The external build request is not a regular file."
        case .invalidSize:
            return "The external build request has an invalid size."
        case .insecurePermissions:
            return "The external build request is not private to this macOS user."
        case .invalidSchema:
            return "The external build request uses an unsupported format."
        case .expired:
            return "The external build request expired. Ask the desktop app to send it again."
        case .invalidRequest:
            return "The external build description is empty or too long."
        case .invalidProjectName:
            return "The external project name is empty or too long."
        }
    }
}
