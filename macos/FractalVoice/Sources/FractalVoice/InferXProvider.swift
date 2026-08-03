import Foundation
import Security

#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// The fixed InferX endpoint and model used by Fractal Voice.
enum InferXProvider {
    static let endpointURL = URL(string: "https://model.inferx.net/endpoints/v1")!
    static let model = "deepseek-v4-flash"
    static let keychainService = "com.fractalsociety.voice.inferx"
    static let keychainAccount = "api-key"
    static let environmentKey = "FRACTAL_INFERX_API_KEY"
    static let enabledEnvironmentKey = "FRACTAL_INFERX_ENABLED"
    static let settingsURL = URL(string: "fractalvoice://provider/inferx")!

    /// Keep the network operation bounded even when a server accepts a
    /// connection but never completes its response.
    static let requestTimeout: TimeInterval = 20

    static var hasStoredAPIKey: Bool {
        keychain.apiKey() != nil
    }

    static func storedAPIKey() -> String? {
        keychain.apiKey()
    }

    static func saveAPIKey(_ rawKey: String) throws {
        try keychain.saveAPIKey(rawKey)
    }

    static func removeAPIKey() throws {
        try keychain.removeAPIKey()
    }

    /// Returns true only for a non-empty, bounded token with no embedded
    /// whitespace or control characters. Leading/trailing whitespace is
    /// intentionally accepted and removed for copy/paste convenience.
    static func normalizedAPIKey(_ rawKey: String) throws -> String {
        let key = rawKey.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty, key.count <= 4096 else {
            throw InferXProviderError.invalidAPIKey
        }
        guard !key.unicodeScalars.contains(where: { scalar in
            CharacterSet.whitespacesAndNewlines.contains(scalar)
                || CharacterSet.controlCharacters.contains(scalar)
        }) else {
            throw InferXProviderError.invalidAPIKey
        }
        return key
    }

    static func isValidAPIKey(_ rawKey: String) -> Bool {
        (try? normalizedAPIKey(rawKey)) != nil
    }

    /// Pure request construction is kept separate from the network call so
    /// tests can verify the payload and authorization header without ever
    /// sending a token over the wire.
    static func makeChatCompletionsRequest(apiKey rawKey: String) throws -> URLRequest {
        let apiKey = try normalizedAPIKey(rawKey)
        let url = endpointURL.appendingPathComponent("chat/completions")
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.timeoutInterval = requestTimeout
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")

        let payload = ChatCompletionsPayload(
            model: model,
            messages: [ChatMessage(role: "user", content: "Reply with OK.")]
        )
        request.httpBody = try JSONEncoder().encode(payload)
        return request
    }

    /// Check the configured key against InferX's OpenAI-compatible endpoint.
    /// Errors intentionally contain only status/category information and
    /// never echo response bodies or credentials.
    static func test(apiKey rawKey: String) async throws {
        let request = try makeChatCompletionsRequest(apiKey: rawKey)
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = requestTimeout
        configuration.timeoutIntervalForResource = requestTimeout
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }

        do {
            let (_, response) = try await session.data(for: request)
            guard let httpResponse = response as? HTTPURLResponse else {
                throw InferXProviderError.invalidResponse
            }
            guard (200..<300).contains(httpResponse.statusCode) else {
                throw InferXProviderError.httpStatus(httpResponse.statusCode)
            }
        } catch let error as InferXProviderError {
            throw error
        } catch let error as URLError where error.code == .timedOut {
            throw InferXProviderError.timeout
        } catch {
            // Do not surface arbitrary URLSession descriptions: they can
            // include server-provided text and are not needed for diagnosis.
            throw InferXProviderError.network
        }
    }

    static func isSettingsURL(_ url: URL) -> Bool {
        guard url.scheme?.lowercased() == "fractalvoice",
              url.host?.lowercased() == "provider"
        else {
            return false
        }
        let path = url.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        return path.lowercased() == "inferx" && url.query == nil && url.fragment == nil
    }

    private static let keychain = InferXKeychain()
}

private struct ChatCompletionsPayload: Encodable {
    let model: String
    let messages: [ChatMessage]
}

private struct ChatMessage: Encodable {
    let role: String
    let content: String
}

/// Keychain access is isolated behind this small value type so production
/// callers never need to pass a token through UserDefaults or a file.
struct InferXKeychain: Sendable {
    func apiKey() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: InferXProvider.keychainService,
            kSecAttrAccount as String: InferXProvider.keychainAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data,
              let rawKey = String(data: data, encoding: .utf8),
              let key = try? InferXProvider.normalizedAPIKey(rawKey)
        else {
            return nil
        }
        return key
    }

    func saveAPIKey(_ rawKey: String) throws {
        let key = try InferXProvider.normalizedAPIKey(rawKey)
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: InferXProvider.keychainService,
            kSecAttrAccount as String: InferXProvider.keychainAccount,
        ]
        SecItemDelete(base as CFDictionary)
        var item = base
        item[kSecValueData as String] = Data(key.utf8)
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else {
            throw InferXProviderError.keychain
        }
    }

    func removeAPIKey() throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: InferXProvider.keychainService,
            kSecAttrAccount as String: InferXProvider.keychainAccount,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw InferXProviderError.keychain
        }
    }
}

enum InferXProviderError: LocalizedError, Equatable {
    case invalidAPIKey
    case keychain
    case timeout
    case invalidResponse
    case httpStatus(Int)
    case network

    var errorDescription: String? {
        switch self {
        case .invalidAPIKey:
            return "Enter a valid InferX API key."
        case .keychain:
            return "The InferX API key could not be saved securely in Keychain."
        case .timeout:
            return "InferX did not respond before the request timed out."
        case .invalidResponse:
            return "InferX returned an invalid response."
        case .httpStatus(let status):
            return "InferX returned HTTP \(status)."
        case .network:
            return "InferX could not be reached. Check your network connection."
        }
    }
}
