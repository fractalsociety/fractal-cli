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

    func show() {
        model.isBuilding = false
        position()
        window.orderFrontRegardless()
    }

    func showBuilding() {
        model.isBuilding = true
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
    @Published var isBuilding = false
}

private struct RecordingHUDView: View {
    @ObservedObject var model: HUDModel
    @State private var pulse = false

    var body: some View {
        HStack(spacing: 16) {
            ZStack {
                Circle()
                    .fill(model.isBuilding ? Color.indigo.opacity(0.2) : Color.red.opacity(0.18))
                    .frame(width: pulse ? 50 : 38, height: pulse ? 50 : 38)
                Image(systemName: model.isBuilding ? "hammer.fill" : "waveform")
                    .font(.system(size: 20, weight: .semibold))
                    .foregroundStyle(model.isBuilding ? .indigo : .red)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(model.isBuilding ? "Starting your build" : "Fractal is listening")
                    .font(.headline)
                Text(model.isBuilding ? "Transcribing locally…" : "Press ⌃⌥Space again to stop")
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
}
