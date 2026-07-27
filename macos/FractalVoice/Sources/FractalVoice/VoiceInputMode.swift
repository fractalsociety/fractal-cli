import Foundation

enum VoiceInputMode: String, CaseIterable, Identifiable {
    static let defaultsKey = "FractalVoice.InputMode.v1"

    case chatGPTDesktop = "chatgpt-desktop"
    case superwhisper
    case builtIn = "built-in"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .chatGPTDesktop: return "ChatGPT Desktop voice"
        case .superwhisper: return "Superwhisper"
        case .builtIn: return "Built-in offline voice"
        }
    }

    var requiresLocalModels: Bool { self == .builtIn }

    func isReady(localModelsReady: Bool) -> Bool {
        !requiresLocalModels || localModelsReady
    }

    static func selected(in defaults: UserDefaults = .standard) -> VoiceInputMode? {
        guard let rawValue = defaults.string(forKey: defaultsKey) else { return nil }
        return VoiceInputMode(rawValue: rawValue)
    }

    static func save(_ mode: VoiceInputMode, in defaults: UserDefaults = .standard) {
        defaults.set(mode.rawValue, forKey: defaultsKey)
    }
}
