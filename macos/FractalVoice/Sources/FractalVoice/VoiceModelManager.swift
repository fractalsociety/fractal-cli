import CryptoKit
import Foundation

enum VoiceModelInstallState: Equatable {
    case checking
    case downloading
    case ready
    case failed(String)
}

struct VoiceEngineConfiguration: Codable, Equatable {
    var schema = "fractal.voice_engine.v1"
    var transcriptionProvider = "granite-local"
    var speechProvider = "kokoro-local"
    var customTranscriptionModel: String?
    var customSpeechModel: String?
    var apiProvider: String?
}

@MainActor
final class VoiceModelManager: ObservableObject {
    @Published private(set) var state: VoiceModelInstallState = .checking
    @Published private(set) var progress = 0.0
    @Published private(set) var currentFile = "Checking voice models…"
    @Published private(set) var downloadedBytes: Int64 = 0
    @Published private(set) var totalBytes: Int64 = assets.reduce(0) { $0 + $1.size }

    private var installing = false

    var isReady: Bool { state == .ready }

    func startIfNeeded() {
        guard !installing, state != .ready else { return }
        installing = true
        state = .checking
        Task.detached(priority: .userInitiated) {
            do {
                try Self.installDefaultConfiguration()
                try Self.installAssets { completed, total, file in
                    Task { @MainActor in
                        self.downloadedBytes = completed
                        self.totalBytes = total
                        self.progress = total > 0 ? Double(completed) / Double(total) : 0
                        self.currentFile = file
                        self.state = .downloading
                    }
                }
                await MainActor.run {
                    self.installing = false
                    self.downloadedBytes = self.totalBytes
                    self.progress = 1
                    self.currentFile = "Offline voice engine installed"
                    self.state = .ready
                }
            } catch {
                await MainActor.run {
                    self.installing = false
                    self.state = .failed(error.localizedDescription)
                }
            }
        }
    }

    func retry() {
        state = .checking
        startIfNeeded()
    }

    nonisolated static var modelRoot: URL {
        AppRuntime.modelRoot
    }

    nonisolated static var graniteDirectory: URL {
        modelRoot.appendingPathComponent("granite-speech-4.1-2b-q4", isDirectory: true)
    }

    nonisolated static var kokoroDirectory: URL {
        modelRoot.appendingPathComponent("kokoro-82m-bf16", isDirectory: true)
    }

    private struct Asset {
        let name: String
        let directory: URL
        let url: URL
        let size: Int64
        let sha256: String
    }

    private nonisolated static let assets: [Asset] = [
        Asset(
            name: "Granite Speech 4.1",
            directory: graniteDirectory,
            url: URL(string: "https://huggingface.co/ibm-granite/granite-speech-4.1-2b-GGUF/resolve/8267dad2adc84209b0efd2702ec68a98356125eb/granite-speech-4.1-2b-Q4_K_M.gguf")!,
            size: 1_139_247_200,
            sha256: "d18e3e79826c4f0fa6734eb05d2db3f06baccbcd5791a83653f946b3178b35d8"
        ),
        Asset(
            name: "Granite audio projector",
            directory: graniteDirectory,
            url: URL(string: "https://huggingface.co/ibm-granite/granite-speech-4.1-2b-GGUF/resolve/8267dad2adc84209b0efd2702ec68a98356125eb/mmproj-model-f16.gguf")!,
            size: 1_159_354_752,
            sha256: "0d3615076cbe1d35c3f60c43a60a4047b3e2eeee1b2c233580be60186faab5c5"
        ),
        Asset(
            name: "Kokoro speech",
            directory: kokoroDirectory,
            url: URL(string: "https://huggingface.co/mlx-community/Kokoro-82M-bf16/resolve/a71e4d38b236d968966a2002c4c895dbd12b1c3c/kokoro-v1_0.safetensors")!,
            size: 327_115_152,
            sha256: "4e9ecdf03b8b6cf906070390237feda473dc13327cb8d56a43deaa374c02acd8"
        ),
        Asset(
            name: "Kokoro voice",
            directory: kokoroDirectory,
            url: URL(string: "https://huggingface.co/mlx-community/Kokoro-82M-bf16/resolve/a71e4d38b236d968966a2002c4c895dbd12b1c3c/voices/af_heart.safetensors")!,
            size: 522_320,
            sha256: "2c1c733b0e6576c810e268d3e440c21dea4e0f0131a3ba4cfc98d7fe6136d094"
        ),
    ]

    private nonisolated static func installAssets(
        progress: @escaping @Sendable (Int64, Int64, String) -> Void
    ) throws {
        let total = assets.reduce(0) { $0 + $1.size }
        var completed: Int64 = 0
        for asset in assets {
            try FileManager.default.createDirectory(
                at: asset.directory,
                withIntermediateDirectories: true
            )
            let destination = asset.directory.appendingPathComponent(asset.url.lastPathComponent)
            if fileIsValid(destination, asset: asset) {
                completed += asset.size
                progress(completed, total, "\(asset.name) verified")
                continue
            }
            try? FileManager.default.removeItem(at: destination)
            let partial = destination.appendingPathExtension("download")
            progress(completed + fileSize(partial), total, "Downloading \(asset.name)…")

            let curl = Process()
            curl.executableURL = URL(fileURLWithPath: "/usr/bin/curl")
            curl.arguments = [
                "--fail", "--location", "--retry", "3", "--continue-at", "-",
                "--output", partial.path, asset.url.absoluteString,
            ]
            curl.standardInput = FileHandle.nullDevice
            curl.standardOutput = FileHandle.nullDevice
            curl.standardError = FileHandle.nullDevice
            try curl.run()
            while curl.isRunning {
                progress(
                    min(total, completed + fileSize(partial)),
                    total,
                    "Downloading \(asset.name)…"
                )
                Thread.sleep(forTimeInterval: 0.35)
            }
            guard curl.terminationStatus == 0 else {
                throw VoiceModelError.downloadFailed(asset.name)
            }
            guard sha256(partial) == asset.sha256 else {
                try? FileManager.default.removeItem(at: partial)
                throw VoiceModelError.checksumFailed(asset.name)
            }
            try FileManager.default.moveItem(at: partial, to: destination)
            completed += asset.size
            progress(completed, total, "\(asset.name) verified")
        }
    }

    private nonisolated static func fileIsValid(_ url: URL, asset: Asset) -> Bool {
        fileSize(url) == asset.size && sha256(url) == asset.sha256
    }

    private nonisolated static func fileSize(_ url: URL) -> Int64 {
        (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(Int64.init) ?? 0
    }

    private nonisolated static func sha256(_ url: URL) -> String {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return "" }
        defer { try? handle.close() }
        var hasher = SHA256()
        while let data = try? handle.read(upToCount: 8 * 1_024 * 1_024), !data.isEmpty {
            hasher.update(data: data)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    private nonisolated static func installDefaultConfiguration() throws {
        let directory = modelRoot.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appendingPathComponent("voice-engine.json")
        if !FileManager.default.fileExists(atPath: url.path) {
            try JSONEncoder().encode(VoiceEngineConfiguration()).write(to: url, options: .atomic)
        }
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o600],
            ofItemAtPath: url.path
        )
    }
}

private enum VoiceModelError: LocalizedError {
    case downloadFailed(String)
    case checksumFailed(String)

    var errorDescription: String? {
        switch self {
        case .downloadFailed(let name):
            return "\(name) could not be downloaded. Check your connection and retry."
        case .checksumFailed(let name):
            return "\(name) failed its security checksum and was removed. Retry the download."
        }
    }
}
