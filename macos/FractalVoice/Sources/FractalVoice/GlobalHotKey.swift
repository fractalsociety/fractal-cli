import Carbon.HIToolbox
import Foundation

/// A system-wide Carbon hotkey. Unlike keyboard event taps, this API does not
/// require Accessibility or Input Monitoring permission.
final class GlobalHotKey {
    static let displayName = "⌥Space"
    static let keyCode = UInt32(kVK_Space)
    static let modifiers = UInt32(optionKey)

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?
    private let action: () -> Void

    init(action: @escaping () -> Void) throws {
        self.action = action

        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        let handlerStatus = InstallEventHandler(
            GetApplicationEventTarget(),
            { _, _, pointer in
                guard let pointer else { return OSStatus(eventNotHandledErr) }
                let instance = Unmanaged<GlobalHotKey>.fromOpaque(pointer).takeUnretainedValue()
                DispatchQueue.main.async {
                    instance.action()
                }
                return noErr
            },
            1,
            &eventType,
            Unmanaged.passUnretained(self).toOpaque(),
            &handler
        )
        guard handlerStatus == noErr else {
            throw GlobalHotKeyError.installHandler(handlerStatus)
        }

        let identifier = EventHotKeyID(signature: 0x4652_4354, id: 1) // FRCT
        let registrationStatus = RegisterEventHotKey(
            Self.keyCode,
            Self.modifiers,
            identifier,
            GetApplicationEventTarget(),
            0,
            &hotKey
        )
        guard registrationStatus == noErr, hotKey != nil else {
            if let handler {
                RemoveEventHandler(handler)
                self.handler = nil
            }
            throw GlobalHotKeyError.register(registrationStatus)
        }
    }

    deinit {
        if let hotKey {
            UnregisterEventHotKey(hotKey)
        }
        if let handler {
            RemoveEventHandler(handler)
        }
    }
}

enum GlobalHotKeyError: LocalizedError {
    case installHandler(OSStatus)
    case register(OSStatus)

    var errorDescription: String? {
        switch self {
        case .installHandler(let status):
            return "macOS could not install the shortcut handler (error \(status))."
        case .register(let status):
            return "⌥Space is already reserved by macOS or another application (error \(status))."
        }
    }
}
