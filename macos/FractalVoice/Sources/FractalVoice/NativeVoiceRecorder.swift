import Foundation
import MoonshineVoice

final class NativeVoiceRecorder: NSObject, TranscriptEventListener, @unchecked Sendable {
    private let transcriber: MicTranscriber
    private let lock = NSLock()
    private var lines: [UInt64: String] = [:]
    private var isClosed = false
    var onPartialTranscript: ((String) -> Void)?
    var onError: ((String) -> Void)?

    init(modelURL: URL) throws {
        transcriber = try MicTranscriber(
            modelPath: modelURL.path,
            modelArch: .mediumStreaming,
            updateInterval: 0.25
        )
        super.init()
        transcriber.addListener(self)
    }

    deinit {
        close()
    }

    func start() throws {
        lock.withLock {
            lines.removeAll(keepingCapacity: true)
        }
        try transcriber.start()
    }

    func stop() throws -> String {
        try transcriber.stop()
        return lock.withLock {
            lines
                .sorted { $0.key < $1.key }
                .map(\.value)
                .filter { !$0.isEmpty }
                .joined(separator: " ")
                .trimmingCharacters(in: .whitespacesAndNewlines)
        }
    }

    func close() {
        let shouldClose = lock.withLock {
            if isClosed {
                return false
            }
            isClosed = true
            return true
        }
        guard shouldClose else { return }
        transcriber.removeListener(self)
        transcriber.close()
    }

    func onLineTextChanged(_ event: LineTextChanged) {
        record(event.line)
        let partial = currentTranscript()
        DispatchQueue.main.async { [weak self] in
            self?.onPartialTranscript?(partial)
        }
    }

    func onLineCompleted(_ event: LineCompleted) {
        record(event.line)
    }

    func onError(_ event: TranscriptError) {
        let message = event.error.localizedDescription
        DispatchQueue.main.async { [weak self] in
            self?.onError?(message)
        }
    }

    private func record(_ line: TranscriptLine) {
        let text = line.text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        lock.withLock {
            lines[line.lineId] = text
        }
    }

    private func currentTranscript() -> String {
        lock.withLock {
            lines
                .sorted { $0.key < $1.key }
                .map(\.value)
                .joined(separator: " ")
        }
    }
}
