import AppKit
import SwiftUI

@MainActor
final class RecordingHUD {
    private let window: NSPanel
    private let model: HUDModel

    init(onStop: @escaping () -> Void, onRestart: @escaping () -> Void) {
        model = HUDModel(onStop: onStop, onRestart: onRestart)
        let content = RecordingHUDView(model: model)
        window = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 148),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        window.level = .floating
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = true
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        window.becomesKeyOnlyIfNeeded = true
        window.contentView = NSHostingView(rootView: content)
    }

    func showPreparing() {
        model.phase = .preparing
        model.summary = "Loading Moonshine v2 Medium…"
        position()
        window.orderFrontRegardless()
    }

    func showListening() {
        model.phase = .listening
        model.summary = "Press ⌃⌥Space again to stop and build"
    }

    func showBuilding(summary: String = "Finishing the local transcript…") {
        model.phase = .building
        model.summary = summary
    }

    func showStopping(restarting: Bool) {
        model.phase = .stopping
        model.summary = restarting
            ? "Pausing this attempt, then reopening the microphone…"
            : "Preserving completed graph waves and releasing active agents…"
    }

    func updateBuilding(summary: String) {
        guard model.phase == .building else { return }
        model.summary = summary
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
    @Published var summary = "Loading Moonshine v2 Medium…"
    let onStop: () -> Void
    let onRestart: () -> Void

    init(onStop: @escaping () -> Void, onRestart: @escaping () -> Void) {
        self.onStop = onStop
        self.onRestart = onRestart
    }
}

private enum HUDPhase: Equatable {
    case preparing
    case listening
    case building
    case stopping
}

private struct RecordingHUDView: View {
    @ObservedObject var model: HUDModel
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 12) {
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
                    Text(model.summary)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            if model.phase == .building || model.phase == .stopping {
                HStack(spacing: 10) {
                    Button(action: model.onStop) {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.bordered)
                    .disabled(model.phase == .stopping)
                    Button(action: model.onRestart) {
                        Label("Restart voice", systemImage: "arrow.counterclockwise")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(model.phase == .stopping)
                    Spacer()
                    Text("Updates every ~15 seconds")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
        }
        .padding(.horizontal, 20)
        .frame(width: 470, height: 138)
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
        model.phase == .listening ? .red : model.phase == .stopping ? .orange : .indigo
    }

    private var icon: String {
        switch model.phase {
        case .preparing: return "sparkles"
        case .listening: return "waveform"
        case .building: return "hammer.fill"
        case .stopping: return "stop.circle.fill"
        }
    }

    private var title: String {
        switch model.phase {
        case .preparing: return "Starting offline voice"
        case .listening: return "Fractal is listening"
        case .building: return "Fractal is building"
        case .stopping: return "Stopping safely"
        }
    }
}
