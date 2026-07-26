import AVFoundation
import Foundation
import KokoroSwift
import MLX

/// Offline speech output for the short confirmation prompts in Fractal Voice.
/// The model and one voice embedding are bundled with the signed application.
final class KokoroSpeaker {
    private let synthesisQueue = DispatchQueue(
        label: "com.fractalsociety.voice.kokoro",
        qos: .userInitiated
    )
    private var tts: KokoroTTS?
    private var voice: MLXArray?
    private var audioEngine: AVAudioEngine?
    private var player: AVAudioPlayerNode?
    private var generation = 0

    func speak(_ text: String, completion: @escaping (Result<Void, Error>) -> Void) {
        stop()
        generation += 1
        let requestedGeneration = generation
        synthesisQueue.async { [weak self] in
            guard let self else { return }
            do {
                let assets = try Self.assets()
                if self.tts == nil {
                    self.tts = KokoroTTS(modelPath: assets.model)
                }
                if self.voice == nil {
                    guard let embedding = try MLX.loadArrays(url: assets.voice)["voice"] else {
                        throw KokoroSpeakerError.voiceEmbeddingMissing
                    }
                    self.voice = embedding
                }
                guard
                    requestedGeneration == self.generation,
                    let tts = self.tts,
                    let voice = self.voice
                else {
                    return
                }
                let (samples, _) = try tts.generateAudio(
                    voice: voice,
                    language: .enUS,
                    text: text
                )
                DispatchQueue.main.async { [weak self] in
                    guard
                        let self,
                        requestedGeneration == self.generation
                    else {
                        return
                    }
                    do {
                        try self.play(
                            samples,
                            generation: requestedGeneration,
                            completion: completion
                        )
                    } catch {
                        completion(.failure(error))
                    }
                }
            } catch {
                DispatchQueue.main.async {
                    completion(.failure(error))
                }
            }
        }
    }

    func stop() {
        generation += 1
        DispatchQueue.main.async { [weak self] in
            self?.player?.stop()
            self?.audioEngine?.stop()
            self?.player = nil
            self?.audioEngine = nil
        }
    }

    private func play(
        _ samples: [Float],
        generation requestedGeneration: Int,
        completion: @escaping (Result<Void, Error>) -> Void
    ) throws {
        guard !samples.isEmpty else {
            throw KokoroSpeakerError.noAudio
        }
        let engine = AVAudioEngine()
        let player = AVAudioPlayerNode()
        engine.attach(player)
        guard let format = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: Double(KokoroTTS.Constants.samplingRate),
            channels: 1,
            interleaved: false
        ), let buffer = AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(samples.count)
        ) else {
            throw KokoroSpeakerError.audioBuffer
        }
        buffer.frameLength = buffer.frameCapacity
        guard let channel = buffer.floatChannelData?[0] else {
            throw KokoroSpeakerError.audioBuffer
        }
        samples.withUnsafeBufferPointer { source in
            channel.update(from: source.baseAddress!, count: samples.count)
        }
        engine.connect(player, to: engine.mainMixerNode, format: format)
        try engine.start()
        self.audioEngine = engine
        self.player = player
        player.scheduleBuffer(
            buffer,
            completionCallbackType: .dataPlayedBack
        ) { [weak self] _ in
            DispatchQueue.main.async {
                guard
                    let self,
                    requestedGeneration == self.generation
                else {
                    return
                }
                self.player?.stop()
                self.audioEngine?.stop()
                self.player = nil
                self.audioEngine = nil
                completion(.success(()))
            }
        }
        player.play()
    }

    static func assets() throws -> (model: URL, voice: URL) {
        guard let resources = Bundle.main.resourceURL else {
            throw KokoroSpeakerError.assetsMissing
        }
        let directory = resources.appendingPathComponent(
            "KokoroModels/Kokoro-82M-bf16",
            isDirectory: true
        )
        let model = directory.appendingPathComponent("kokoro-v1_0.safetensors")
        let voice = directory.appendingPathComponent("af_heart.safetensors")
        guard
            FileManager.default.fileExists(atPath: model.path),
            FileManager.default.fileExists(atPath: voice.path)
        else {
            throw KokoroSpeakerError.assetsMissing
        }
        return (model, voice)
    }

    /// Loads the exact bundled weights and produces a short buffer without
    /// playing it. Used by the release/install smoke test.
    static func synthesisSelfTest() throws {
        let assets = try assets()
        let tts = KokoroTTS(modelPath: assets.model)
        guard let voice = try MLX.loadArrays(url: assets.voice)["voice"] else {
            throw KokoroSpeakerError.voiceEmbeddingMissing
        }
        let (samples, _) = try tts.generateAudio(
            voice: voice,
            language: .enUS,
            text: "Fractal Voice is ready."
        )
        guard !samples.isEmpty else {
            throw KokoroSpeakerError.noAudio
        }
    }
}

private enum KokoroSpeakerError: LocalizedError {
    case assetsMissing
    case voiceEmbeddingMissing
    case noAudio
    case audioBuffer

    var errorDescription: String? {
        switch self {
        case .assetsMissing:
            return "The bundled Kokoro 82M speech model is missing."
        case .voiceEmbeddingMissing:
            return "The bundled Kokoro voice could not be loaded."
        case .noAudio:
            return "Kokoro did not generate any speech audio."
        case .audioBuffer:
            return "The Kokoro speech audio buffer could not be created."
        }
    }
}
