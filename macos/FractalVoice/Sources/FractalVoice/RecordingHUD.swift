import AppKit
import SwiftUI

@MainActor
final class RecordingHUD {
    private let window: NSPanel
    private let model = HUDModel()

    init() {
        let content = RecordingHUDView(model: model)
        window = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 390, height: 92),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        window.level = .floating
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = true
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.contentView = NSHostingView(rootView: content)
    }

    func showPreparing() {
        model.phase = .preparing
        position()
        window.orderFrontRegardless()
    }

    func showListening() {
        model.phase = .listening
    }

    func showBuilding() {
        model.phase = .building
    }

    func close() {
        window.close()
    }

    private func position() {
        guard let screen = NSScreen.main else { return }
        let frame = screen.visibleFrame
        window.setFrameOrigin(NSPoint(
            x: frame.midX - window.frame.width / 2,
            y: frame.minY + 36
        ))
    }
}

@MainActor
private final class HUDModel: ObservableObject {
    @Published var phase: HUDPhase = .preparing
}

private enum HUDPhase {
    case preparing
    case listening
    case building
}

private struct RecordingHUDView: View {
    @ObservedObject var model: HUDModel
    @State private var pulse = false

    var body: some View {
        HStack(spacing: 16) {
            ZStack {
                Circle()
                    .fill(accent.opacity(0.18))
                    .frame(width: pulse ? 50 : 38, height: pulse ? 50 : 38)
                Image(systemName: icon)
                    .font(.system(size: 20, weight: .semibold))
                    .foregroundStyle(accent)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.headline)
                Text(detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.horizontal, 20)
        .frame(width: 390, height: 82)
        .background(.ultraThickMaterial, in: RoundedRectangle(cornerRadius: 22))
        .overlay(
            RoundedRectangle(cornerRadius: 22)
                .stroke(Color.white.opacity(0.25), lineWidth: 1)
        )
        .padding(5)
        .onAppear {
            withAnimation(.easeInOut(duration: 0.8).repeatForever(autoreverses: true)) {
                pulse = true
            }
        }
    }

    private var accent: Color {
        model.phase == .listening ? .red : .indigo
    }

    private var icon: String {
        switch model.phase {
        case .preparing: return "sparkles"
        case .listening: return "waveform"
        case .building: return "hammer.fill"
        }
    }

    private var title: String {
        switch model.phase {
        case .preparing: return "Starting offline voice"
        case .listening: return "Fractal is listening"
        case .building: return "Starting your build"
        }
    }

    private var detail: String {
        switch model.phase {
        case .preparing: return "Loading Moonshine v2 Medium…"
        case .listening: return "Press ⌃⌥Space again to stop"
        case .building: return "Finishing the local transcript…"
        }
    }
}
