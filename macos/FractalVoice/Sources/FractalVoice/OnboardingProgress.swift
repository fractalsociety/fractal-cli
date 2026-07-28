import Foundation

struct OnboardingProgress {
    static let currentSchemaVersion = 2
    static let schemaVersionKey = "completedOnboardingSchemaVersion"

    static func isComplete(
        defaults: UserDefaults = .standard,
        requiredSchemaVersion: Int = currentSchemaVersion
    ) -> Bool {
        defaults.integer(forKey: schemaVersionKey) >= requiredSchemaVersion
    }

    static func markComplete(
        defaults: UserDefaults = .standard,
        schemaVersion: Int = currentSchemaVersion
    ) {
        defaults.set(schemaVersion, forKey: schemaVersionKey)
        defaults.set(true, forKey: "completedOnboarding")
    }
}
