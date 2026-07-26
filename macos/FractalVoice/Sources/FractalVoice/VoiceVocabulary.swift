import Foundation

struct VoiceVocabularyResult: Equatable {
    let transcript: String
    let appliedCorrections: [String]
}

struct VoiceVocabularyFile: Codable, Equatable {
    static let schema = "fractal.voice-vocabulary.v1"

    var schema: String
    var terms: [String]
    var corrections: [String: String]

    init(
        schema: String = VoiceVocabularyFile.schema,
        terms: [String] = [],
        corrections: [String: String] = [:]
    ) {
        self.schema = schema
        self.terms = terms
        self.corrections = corrections
    }
}

/// A local, deterministic transcript normalization layer.
///
/// It intentionally changes only exact configured phrases. It never asks a
/// generative model to reinterpret a command, and therefore leaves paths,
/// numbers, shell syntax, and unknown identifiers untouched.
struct VoiceVocabularyEngine {
    static let personalRelativePath = ".fractal/voice/vocabulary.json"
    static let projectRelativePath = ".fractal/vocabulary.json"

    private static let builtIn = VoiceVocabularyFile(
        terms: [
            "Fractal",
            "Fractal CLI",
            "Fractal Society",
            "execution graph",
            "PRD",
            "Codex",
            "OpenAI Codex",
            "Cursor",
            "Cursor Agent",
            "Claude",
            "Claude Code",
            "GitHub",
            "Xcode",
            "XcodeGen",
            "SwiftUI",
            "Moonshine",
            "Superwhisper",
            "WhisperKit",
            "Granite Speech",
            "DataEvol",
            "Fractal Coordinate",
            "Fractal Forge",
            "Fractal Voice",
            "HotStuff",
            "Biolatent",
            "LoRA",
            "MLX",
            "GRPO",
            "PPO",
            "macOS",
            "iOS",
            "iPadOS",
            "watchOS",
            "visionOS",
            "AppKit",
            "AVFoundation",
            "Core ML",
            "Metal",
            "MLX Audio",
            "React",
            "React Native",
            "Next.js",
            "Node.js",
            "TypeScript",
            "JavaScript",
            "Rust",
            "Cargo",
            "Python",
            "FastAPI",
            "PostgreSQL",
            "SQLite",
            "GraphQL",
            "WebSocket",
            "Docker",
            "Kubernetes",
            "Homebrew",
            "Hugging Face",
            "llama.cpp",
            "GGUF",
            "Q4_K_M",
            "API",
            "CLI",
            "JSON",
            "YAML",
            "README",
            "DMARC",
            "DNS",
            "SSH",
            "URL",
            "UI",
            "UX",
            "CI/CD"
        ],
        corrections: [
            "fractal sea ally": "Fractal CLI",
            "fractal c l i": "Fractal CLI",
            "fracture cli": "Fractal CLI",
            "fractal society": "Fractal Society",
            "execution graft": "execution graph",
            "execution grab": "execution graph",
            "execution graph": "execution graph",
            "execute asian graph": "execution graph",
            "p r d": "PRD",
            "code x": "Codex",
            "co dex": "Codex",
            "open a i code x": "OpenAI Codex",
            "cursor agent": "Cursor Agent",
            "clawed": "Claude",
            "clawed code": "Claude Code",
            "git hub": "GitHub",
            "x code": "Xcode",
            "x code gen": "XcodeGen",
            "swift u i": "SwiftUI",
            "swift ui": "SwiftUI",
            "moon shine": "Moonshine",
            "super whisper": "Superwhisper",
            "whisper kit": "WhisperKit",
            "granite speech": "Granite Speech",
            "data evolve": "DataEvol",
            "fractal coordinate": "Fractal Coordinate",
            "fractal forge": "Fractal Forge",
            "fractal voice": "Fractal Voice",
            "hot stuff": "HotStuff",
            "bio latent": "Biolatent",
            "low rah": "LoRA",
            "m l x": "MLX",
            "g r p o": "GRPO",
            "p p o": "PPO",
            "mac o s": "macOS",
            "i o s": "iOS",
            "ipad o s": "iPadOS",
            "watch o s": "watchOS",
            "vision o s": "visionOS",
            "app kit": "AppKit",
            "a v foundation": "AVFoundation",
            "core m l": "Core ML",
            "m l x audio": "MLX Audio",
            "react native": "React Native",
            "next j s": "Next.js",
            "node j s": "Node.js",
            "type script": "TypeScript",
            "java script": "JavaScript",
            "fast a p i": "FastAPI",
            "post gre s q l": "PostgreSQL",
            "s q lite": "SQLite",
            "graph q l": "GraphQL",
            "web socket": "WebSocket",
            "home brew": "Homebrew",
            "hugging face": "Hugging Face",
            "llama c p p": "llama.cpp",
            "g g u f": "GGUF",
            "q four k m": "Q4_K_M",
            "a p i": "API",
            "c l i": "CLI",
            "j son": "JSON",
            "yammel": "YAML",
            "read me": "README",
            "d mark": "DMARC",
            "d n s": "DNS",
            "s s h": "SSH",
            "u r l": "URL",
            "u i": "UI",
            "u x": "UX",
            "c i c d": "CI/CD"
        ]
    )

    let personalURL: URL

    init(homeURL: URL = FileManager.default.homeDirectoryForCurrentUser) {
        personalURL = homeURL.appendingPathComponent(
            Self.personalRelativePath,
            isDirectory: false
        )
    }

    func installPersonalTemplateIfNeeded() throws {
        guard !FileManager.default.fileExists(atPath: personalURL.path) else {
            return
        }
        try FileManager.default.createDirectory(
            at: personalURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let template = VoiceVocabularyFile(
            terms: [],
            corrections: [
                "example mishearing": "Example Product Name"
            ]
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        var data = try encoder.encode(template)
        data.append(0x0A)
        try data.write(to: personalURL, options: .atomic)
    }

    func normalize(_ transcript: String, projectURL: URL? = nil) -> VoiceVocabularyResult {
        let vocabularies = [
            Self.builtIn,
            loadVocabulary(at: personalURL),
            projectURL.map {
                loadVocabulary(
                    at: $0.appendingPathComponent(Self.projectRelativePath)
                )
            } ?? nil
        ].compactMap { $0 }

        var corrections = Self.builtIn.corrections
        for vocabulary in vocabularies.dropFirst() {
            corrections.merge(vocabulary.corrections) { _, userValue in userValue }
        }

        var normalized = transcript
        var applied: [String] = []
        for source in corrections.keys.sorted(by: Self.correctionOrder) {
            guard let replacement = corrections[source] else { continue }
            let outcome = Self.replacingPhrase(
                source,
                with: replacement,
                in: normalized
            )
            if outcome.didReplace {
                normalized = outcome.text
                applied.append("\(source) → \(replacement)")
            }
        }

        let terms = vocabularies
            .flatMap(\.terms)
            .reduce(into: [String: String]()) { result, term in
                let key = term.folding(
                    options: [.caseInsensitive, .diacriticInsensitive],
                    locale: .current
                )
                result[key] = term
            }
            .values
            .sorted(by: Self.correctionOrder)

        for term in terms {
            let outcome = Self.replacingPhrase(term, with: term, in: normalized)
            if outcome.didReplace, outcome.text != normalized {
                normalized = outcome.text
                applied.append("canonicalized \(term)")
            }
        }

        return VoiceVocabularyResult(
            transcript: normalized.trimmingCharacters(in: .whitespacesAndNewlines),
            appliedCorrections: applied
        )
    }

    func promptTerms(projectURL: URL? = nil) -> [String] {
        var terms = Self.builtIn.terms
        terms += loadVocabulary(at: personalURL)?.terms ?? []
        if let projectURL {
            terms += loadVocabulary(
                at: projectURL.appendingPathComponent(Self.projectRelativePath)
            )?.terms ?? []
        }
        return Array(Set(terms)).sorted()
    }

    private func loadVocabulary(at url: URL) -> VoiceVocabularyFile? {
        guard
            let data = try? Data(contentsOf: url),
            let vocabulary = try? JSONDecoder().decode(VoiceVocabularyFile.self, from: data),
            vocabulary.schema == VoiceVocabularyFile.schema
        else {
            return nil
        }
        return vocabulary
    }

    private static func correctionOrder(_ lhs: String, _ rhs: String) -> Bool {
        if lhs.count != rhs.count {
            return lhs.count > rhs.count
        }
        return lhs.localizedCaseInsensitiveCompare(rhs) == .orderedAscending
    }

    private static func replacingPhrase(
        _ phrase: String,
        with replacement: String,
        in input: String
    ) -> (text: String, didReplace: Bool) {
        let tokens = phrase.split(whereSeparator: \.isWhitespace)
        guard !tokens.isEmpty else { return (input, false) }
        let escaped = tokens
            .map { NSRegularExpression.escapedPattern(for: String($0)) }
            .joined(separator: #"\s+"#)
        let pattern = #"(?<![\p{L}\p{N}_])\#(escaped)(?![\p{L}\p{N}_])"#
        guard let regex = try? NSRegularExpression(
            pattern: pattern,
            options: [.caseInsensitive]
        ) else {
            return (input, false)
        }
        let range = NSRange(input.startIndex..., in: input)
        let matches = regex.matches(in: input, range: range)
        guard !matches.isEmpty else { return (input, false) }

        var output = input
        for match in matches.reversed() {
            guard let swiftRange = Range(match.range, in: output) else { continue }
            output.replaceSubrange(swiftRange, with: replacement)
        }
        return (output, true)
    }
}
