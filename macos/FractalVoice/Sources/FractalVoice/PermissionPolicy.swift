import Foundation

/// Permission requests are tied to the user action that needs the capability.
/// Startup and external text handoffs must stay silent so a text build never
/// activates the voice permission path.
enum PermissionRequestContext: Equatable {
    case appLaunch
    case externalTextHandoff
    case explicitVoiceRecording
    case explicitNotificationOptIn
    case backgroundBuildStatus
}

enum PermissionPolicy {
    static func shouldRequestMicrophone(
        in context: PermissionRequestContext
    ) -> Bool {
        context == .explicitVoiceRecording
    }

    static func shouldRequestNotifications(
        in context: PermissionRequestContext
    ) -> Bool {
        context == .explicitNotificationOptIn
    }
}
