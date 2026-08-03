import SwiftUI

/// Native settings for the optional InferX provider. The API key is never
/// mirrored into UserDefaults or view persistence; the SecureField's value is
/// sent directly to the provider and then stored in Keychain on success.
struct InferXSettingsView: View {
    @State private var apiKey = ""
    @State private var status = "No InferX API key is saved."
    @State private var hasStoredKey = false
    @State private var isTesting = false

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("InferX API Key")
                .font(.title2.weight(.semibold))

            Text("Use InferX for Fractal CLI builds. Your key is stored only in the macOS Keychain.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            GroupBox {
                VStack(alignment: .leading, spacing: 8) {
                    LabeledContent("Endpoint") {
                        Text(InferXProvider.endpointURL.absoluteString)
                            .textSelection(.enabled)
                            .foregroundStyle(.secondary)
                    }
                    LabeledContent("Model") {
                        Text(InferXProvider.model)
                            .textSelection(.enabled)
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(4)
            }

            SecureField("Paste API key", text: $apiKey)
                .textFieldStyle(.roundedBorder)
                .disabled(isTesting)
                .onSubmit { saveAndTest() }

            HStack(spacing: 10) {
                Button(isTesting ? "Testing…" : "Save & Test") {
                    saveAndTest()
                }
                .buttonStyle(.borderedProminent)
                .disabled(isTesting || apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

                Button("Remove Key") {
                    removeKey()
                }
                .disabled(isTesting || !hasStoredKey)
            }

            Text(status)
                .font(.callout)
                .foregroundStyle(statusIsError ? .red : .secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(24)
        .frame(width: 560)
        .onAppear(perform: refreshStoredKeyStatus)
    }

    private var statusIsError: Bool {
        status.hasPrefix("Error:")
    }

    private func refreshStoredKeyStatus() {
        hasStoredKey = InferXProvider.hasStoredAPIKey
        if hasStoredKey {
            status = "An InferX API key is saved securely in Keychain."
        }
    }

    private func saveAndTest() {
        guard !isTesting else { return }
        isTesting = true
        status = "Testing InferX connection…"
        let rawKey = apiKey
        Task { @MainActor in
            do {
                let key = try InferXProvider.normalizedAPIKey(rawKey)
                try await InferXProvider.test(apiKey: key)
                try InferXProvider.saveAPIKey(key)
                apiKey = ""
                hasStoredKey = true
                status = "Connected to InferX. API key saved securely in Keychain."
            } catch {
                status = "Error: \(sanitizedMessage(for: error))"
            }
            isTesting = false
        }
    }

    private func removeKey() {
        do {
            try InferXProvider.removeAPIKey()
            apiKey = ""
            hasStoredKey = false
            status = "InferX API key removed from Keychain."
        } catch {
            status = "Error: \(sanitizedMessage(for: error))"
        }
    }

    private func sanitizedMessage(for error: Error) -> String {
        if let providerError = error as? InferXProviderError {
            return providerError.localizedDescription
        }
        return "The InferX connection could not be completed."
    }
}

#Preview {
    InferXSettingsView()
}
