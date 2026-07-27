import Foundation
import Security

struct BridgeCommandResult: Decodable, Sendable {
    let ok: Bool
    let exitCode: Int32
    let output: String

    enum CodingKeys: String, CodingKey {
        case ok
        case exitCode = "exit_code"
        case output
    }
}

struct BridgeReadiness: Decodable, Sendable {
    struct Agent: Decodable, Sendable {
        let id: String
        let installed: Bool
        let authenticated: Bool
    }

    let agents: [Agent]
    let gitInstalled: Bool
    let githubCLIInstalled: Bool
    let githubAuthenticated: Bool
    let fractalSocietyAuthenticated: Bool
    let fractalSocietyAccount: String?

    enum CodingKeys: String, CodingKey {
        case agents
        case gitInstalled = "git_installed"
        case githubCLIInstalled = "github_cli_installed"
        case githubAuthenticated = "github_authenticated"
        case fractalSocietyAuthenticated = "fractal_society_authenticated"
        case fractalSocietyAccount = "fractal_society_account"
    }
}

enum LocalBridge {
    static let baseURL = URL(string: "http://127.0.0.1:18372")!
    private static let keychainService = "com.fractalsociety.voice.local-bridge"
    private static let keychainAccount = "pairing-token"
    private static let keychainConsentKey = "FractalVoice.LocalBridgeKeychainConsent.v1"

    /// Keychain reads are deliberately disabled until the user has chosen to
    /// save a bridge token after seeing the in-app purpose explanation. This
    /// prevents a surprising macOS Keychain prompt during automatic setup
    /// checks on first launch.
    static var hasKeychainConsent: Bool {
        UserDefaults.standard.bool(forKey: keychainConsentKey)
    }

    static var pairingToken: String? {
        guard hasKeychainConsent else { return nil }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: keychainService,
            kSecAttrAccount as String: keychainAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data,
              let token = String(data: data, encoding: .utf8),
              token.count >= 48
        else {
            return nil
        }
        return token
    }

    static func savePairingToken(_ rawToken: String) throws {
        let token = rawToken.trimmingCharacters(in: .whitespacesAndNewlines)
        guard token.count >= 48,
              token.utf8.allSatisfy({ byte in
                  (48...57).contains(byte) || (97...102).contains(byte)
              })
        else {
            throw BridgeError.invalidToken
        }
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: keychainService,
            kSecAttrAccount as String: keychainAccount,
        ]
        SecItemDelete(base as CFDictionary)
        var item = base
        item[kSecValueData as String] = Data(token.utf8)
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else {
            throw BridgeError.keychain
        }
        UserDefaults.standard.set(true, forKey: keychainConsentKey)
    }

    static func readiness() throws -> BridgeReadiness {
        try request(path: "/v1/readiness", timeout: 20)
    }

    static func login() throws -> BridgeCommandResult {
        try request(path: "/v1/login", method: "POST", body: Data("{}".utf8), timeout: 330)
    }

    static func build(
        request content: String,
        projectName: String,
        leadAgent: String? = UserDefaults.standard.string(forKey: "selectedLeadAgent")
    ) throws -> BridgeCommandResult {
        let body = try JSONSerialization.data(withJSONObject: [
            "request": content,
            "project_name": projectName,
            "lead_agent": leadAgent ?? "codex",
        ])
        return try request(path: "/v1/build", method: "POST", body: body, timeout: 6 * 60 * 60)
    }

    static func stop(project: String?, all: Bool) throws {
        var payload: [String: Any] = ["all": all]
        if let project {
            payload["project"] = project
        }
        let body = try JSONSerialization.data(withJSONObject: payload)
        let _: BridgeStopResult = try request(
            path: "/v1/stop",
            method: "POST",
            body: body,
            timeout: 20
        )
    }

    static func amend(_ content: String) throws -> BridgeCommandResult {
        let body = try JSONSerialization.data(withJSONObject: ["request": content])
        return try request(path: "/v1/amend", method: "POST", body: body, timeout: 30)
    }

    private static func request<Response: Decodable>(
        path: String,
        method: String = "GET",
        body: Data? = nil,
        timeout: TimeInterval
    ) throws -> Response {
        guard let token = pairingToken else { throw BridgeError.notPaired }
        var request = URLRequest(url: baseURL.appendingPathComponent(path))
        request.httpMethod = method
        request.httpBody = body
        request.timeoutInterval = timeout
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")

        let semaphore = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var capturedData: Data?
        var capturedResponse: URLResponse?
        var capturedError: Error?
        URLSession.shared.dataTask(with: request) { data, response, error in
            lock.lock()
            capturedData = data
            capturedResponse = response
            capturedError = error
            lock.unlock()
            semaphore.signal()
        }.resume()
        guard semaphore.wait(timeout: .now() + timeout + 2) == .success else {
            throw BridgeError.timeout
        }
        lock.lock()
        let data = capturedData
        let response = capturedResponse
        let error = capturedError
        lock.unlock()
        if let error { throw error }
        guard let http = response as? HTTPURLResponse, let data else {
            throw BridgeError.invalidResponse
        }
        guard (200..<300).contains(http.statusCode) else {
            throw BridgeError.http(http.statusCode)
        }
        return try JSONDecoder().decode(Response.self, from: data)
    }
}

private struct BridgeStopResult: Decodable {
    let ok: Bool
}

enum BridgeError: LocalizedError {
    case invalidToken
    case keychain
    case notPaired
    case timeout
    case invalidResponse
    case http(Int)

    var errorDescription: String? {
        switch self {
        case .invalidToken:
            return "The bridge token is invalid. Copy it again from `fractal bridge token`."
        case .keychain:
            return "The bridge token could not be saved securely in Keychain."
        case .notPaired:
            return "Pair Fractal Voice with the local CLI bridge first."
        case .timeout:
            return "The local Fractal bridge did not respond in time."
        case .invalidResponse:
            return "The local Fractal bridge returned an invalid response."
        case .http(let status):
            return "The local Fractal bridge returned HTTP \(status)."
        }
    }
}
