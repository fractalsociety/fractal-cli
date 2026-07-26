import Carbon.HIToolbox
import Foundation

final class GlobalHotKey {
    static let displayName = "⌃⌥Space"

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?
    private let action: () -> Void

    init(action: @escaping () -> Void) {
        self.action = action

        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        InstallEventHandler(
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

        let identifier = EventHotKeyID(signature: 0x4652_4354, id: 1) // FRCT
        RegisterEventHotKey(
            UInt32(kVK_Space),
            UInt32(controlKey | optionKey),
            identifier,
            GetApplicationEventTarget(),
            0,
            &hotKey
        )
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
