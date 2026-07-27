import AVFoundation
import Foundation

/// Captures one voice command to a temporary WAV file for Granite Speech.
final class NativeVoiceRecorder: @unchecked Sendable {
    private let engine = AVAudioEngine()
    private let lock = NSLock()
    private let endingSilenceDuration: Double
    private var audioFile: AVAudioFile?
    private var recordingURL: URL?
    private var isRecording = false
    private var voiceActivity = VoiceActivityDetector()
    var onError: ((String) -> Void)?
    var onSpeechDetected: (() -> Void)?
    var onUtteranceEnded: (() -> Void)?

    init(endingSilenceDuration: Double = VoiceActivityDetector.defaultEndingSilenceDuration) {
        self.endingSilenceDuration = endingSilenceDuration
        voiceActivity = VoiceActivityDetector(
            endingSilenceDuration: endingSilenceDuration,
            calibrationDuration: 0.25
        )
    }

    func start() throws {
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else {
            throw VoiceRecorderError.microphoneUnavailable
        }

        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("fractal-voice-\(UUID().uuidString).wav")
        let file = try AVAudioFile(
            forWriting: url,
            settings: format.settings,
            commonFormat: .pcmFormatFloat32,
            interleaved: false
        )

        lock.withLock {
            audioFile = file
            recordingURL = url
            isRecording = true
            voiceActivity = VoiceActivityDetector(
                endingSilenceDuration: endingSilenceDuration,
                calibrationDuration: 0.25
            )
        }
        input.installTap(
            onBus: 0,
            bufferSize: 4_096,
            format: format
        ) { [weak self] buffer, _ in
            guard let self else { return }
            do {
                try self.lock.withLock {
                    guard self.isRecording else { return }
                    try self.audioFile?.write(from: buffer)
                }
                self.observeVoiceActivity(in: buffer)
            } catch {
                DispatchQueue.main.async { [weak self] in
                    self?.onError?(error.localizedDescription)
                }
            }
        }
        engine.prepare()
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            lock.withLock {
                isRecording = false
                audioFile = nil
            }
            try? FileManager.default.removeItem(at: url)
            throw error
        }
    }

    private func observeVoiceActivity(in buffer: AVAudioPCMBuffer) {
        guard
            let samples = buffer.floatChannelData?[0],
            buffer.frameLength > 0,
            buffer.format.sampleRate > 0
        else {
            return
        }
        var sum: Float = 0
        var peak: Float = 0
        for index in 0..<Int(buffer.frameLength) {
            let sample = samples[index]
            sum += sample * sample
            peak = max(peak, abs(sample))
        }
        let rms = sqrt(sum / Float(buffer.frameLength))
        let duration = Double(buffer.frameLength) / buffer.format.sampleRate
        let event: VoiceActivityEvent? = lock.withLock {
            guard isRecording else { return nil }
            return voiceActivity.observe(rms: rms, peak: peak, duration: duration)
        }
        switch event {
        case .speechStarted:
            DispatchQueue.main.async { [weak self] in
                self?.onSpeechDetected?()
            }
        case .utteranceEnded:
            DispatchQueue.main.async { [weak self] in
                self?.onUtteranceEnded?()
            }
        case nil:
            break
        }
    }

    func stop() throws -> URL {
        engine.stop()
        engine.inputNode.removeTap(onBus: 0)
        let result = try lock.withLock {
            isRecording = false
            audioFile = nil
            guard let recordingURL else {
                throw VoiceRecorderError.noRecording
            }
            self.recordingURL = nil
            return (recordingURL, voiceActivity.heardSpeech)
        }
        guard result.1 else {
            try? FileManager.default.removeItem(at: result.0)
            throw VoiceRecorderError.noSpeech
        }
        return result.0
    }

    func close() {
        if engine.isRunning {
            engine.stop()
            engine.inputNode.removeTap(onBus: 0)
        }
        let url = lock.withLock {
            isRecording = false
            audioFile = nil
            let url = recordingURL
            recordingURL = nil
            return url
        }
        if let url {
            try? FileManager.default.removeItem(at: url)
        }
    }
}

enum VoiceActivityEvent: Equatable {
    case speechStarted
    case utteranceEnded
}

struct VoiceActivityDetector {
    private(set) var heardSpeech = false
    private var candidateSpeechDuration = 0.0
    private var trailingSilenceDuration = 0.0
    private var ended = false
    private var speechReference: Float = 0
    private var observedDuration = 0.0
    private var noiseReference: Float = 0
    private let endingSilenceDuration: Double
    private let calibrationDuration: Double

    static let speechThreshold: Float = 0.0035
    // Require speech-like energy across more than one normal capture buffer.
    // This prevents a click, notification sound, or brief room-noise spike from
    // being sent to the speech model as though it were an utterance.
    static let minimumSpeechDuration = 0.12
    static let defaultEndingSilenceDuration = 0.72

    init(
        endingSilenceDuration: Double = Self.defaultEndingSilenceDuration,
        calibrationDuration: Double = 0
    ) {
        self.endingSilenceDuration = endingSilenceDuration
        self.calibrationDuration = calibrationDuration
    }

    mutating func observe(
        rms: Float,
        peak: Float? = nil,
        duration: Double
    ) -> VoiceActivityEvent? {
        guard !ended else { return nil }
        // Peak awareness catches short/quiet words whose energy is diluted across
        // an audio buffer. RMS remains the main signal so keyboard clicks do not
        // become utterances.
        let level = max(rms, (peak ?? rms) * 0.16)
        observedDuration += duration
        if observedDuration <= calibrationDuration {
            noiseReference = noiseReference == 0
                ? level
                : min(noiseReference, level)
            return nil
        }
        let adaptiveSpeechThreshold = max(
            Self.speechThreshold,
            noiseReference * 2.6
        )
        let adaptiveRMSFloor = max(0.0012, noiseReference * 1.45)
        let releaseThreshold = heardSpeech
            ? max(Self.speechThreshold * 0.72, speechReference * 0.22)
            : adaptiveSpeechThreshold
        if level >= releaseThreshold && rms >= adaptiveRMSFloor {
            trailingSilenceDuration = 0
            speechReference = max(speechReference, level)
            if !heardSpeech {
                candidateSpeechDuration += duration
                if candidateSpeechDuration >= Self.minimumSpeechDuration {
                    heardSpeech = true
                    return .speechStarted
                }
            }
            return nil
        }

        if heardSpeech {
            trailingSilenceDuration += duration
            if trailingSilenceDuration >= endingSilenceDuration {
                ended = true
                return .utteranceEnded
            }
        } else {
            candidateSpeechDuration = 0
            noiseReference = noiseReference == 0
                ? level
                : noiseReference * 0.96 + level * 0.04
        }
        return nil
    }
}

private enum VoiceRecorderError: LocalizedError {
    case microphoneUnavailable
    case noRecording
    case noSpeech

    var errorDescription: String? {
        switch self {
        case .microphoneUnavailable:
            return "The microphone did not provide a usable audio format."
        case .noRecording:
            return "No voice recording was available to transcribe."
        case .noSpeech:
            return "No speech was detected. Please speak after the listening indicator appears."
        }
    }
}
