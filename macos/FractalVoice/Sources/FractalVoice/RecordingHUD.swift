import AppKit
import SwiftUI

@MainActor
final class RecordingHUD {
    private let window: NSPanel
    private let model: HUDModel

    init(
        onStop: @escaping () -> Void,
        onRestart: @escaping () -> Void,
        onYes: @escaping () -> Void = {},
        onNo: @escaping () -> Void = {},
        onTypeInstead: @escaping () -> Void = {},
        onManualRequest: @escaping (String) -> Void = { _ in },
        onManualName: @escaping (String) -> Void = { _ in }
    ) {
        model = HUDModel(
            onStop: onStop,
            onRestart: onRestart,
            onYes: onYes,
            onNo: onNo,
            onTypeInstead: onTypeInstead,
            onManualRequest: onManualRequest,
            onManualName: onManualName
        )
        let content = RecordingHUDView(model: model)
        window = InputPanel(
            contentRect: NSRect(x: 0, y: 0, width: 590, height: 230),
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
        model.summary = "Starting Granite Speech and Kokoro locally…"
        position()
        window.orderFrontRegardless()
    }

    func showListening(summary: String = "Press ⌥Space again to stop") {
        model.phase = .listening
        model.summary = summary
        model.manualText = ""
    }

    func showManualRequest() {
        model.phase = .manualRequest
        model.summary = "Type exactly what you want Fractal to build."
        model.manualText = ""
        position()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        focusManualInput()
    }

    func showBuilding(summary: String = "Finishing the local transcript…") {
        model.phase = .building
        model.summary = summary
    }

    func showTranscribing() {
        model.phase = .transcribing
        model.summary = "Applying product vocabulary and accurate local transcription…"
    }

    func showQuestion(_ question: String) {
        model.phase = .question
        model.summary = question
        position()
        window.orderFrontRegardless()
    }

    func showNaming(_ summary: String) {
        let wasNaming = model.phase == .naming
        model.phase = .naming
        model.summary = summary
        if !wasNaming {
            model.manualText = ""
        }
        position()
        window.makeKeyAndOrderFront(nil)
        focusManualInput()
    }

    func showStopping(restarting: Bool) {
        model.phase = .stopping
        model.summary = restarting
            ? "Pausing this attempt, then reopening the microphone…"
            : "Preserving completed graph waves and releasing active agents…"
    }

    func updateBuilding(summary: String) {
        guard model.phase == .building || model.phase == .transcribing else {
            return
        }
        model.phase = .building
        model.summary = summary
    }

    func close() {
        window.close()
    }

    var isShowingBuildProgressForTesting: Bool {
        model.phase == .building
    }

    var isShowingQuestionForTesting: Bool {
        model.phase == .question
    }

    var summaryForTesting: String {
        model.summary
    }

    var isShowingManualRequestForTesting: Bool {
        model.phase == .manualRequest
    }

    func submitManualTextForTesting(_ text: String) {
        model.manualText = text
        model.submitManual()
    }

    private func position() {
        guard let screen = NSScreen.main else { return }
        let frame = screen.visibleFrame
        window.setFrameOrigin(NSPoint(
            x: frame.midX - window.frame.width / 2,
            y: frame.minY + 36
        ))
    }

    private func focusManualInput() {
        DispatchQueue.main.async { [weak self] in
            guard let self, let contentView = self.window.contentView else { return }
            self.window.makeFirstResponder(Self.firstTextField(in: contentView))
        }
    }

    private static func firstTextField(in view: NSView) -> NSTextField? {
        if let field = view as? NSTextField {
            return field
        }
        for child in view.subviews {
            if let field = firstTextField(in: child) {
                return field
            }
        }
        return nil
    }
}

private final class InputPanel: NSPanel {
    override var canBecomeKey: Bool { true }
}

@MainActor
private final class HUDModel: ObservableObject {
    @Published var phase: HUDPhase = .preparing
    @Published var summary = "Starting offline voice…"
    let onStop: () -> Void
    let onRestart: () -> Void
    let onYes: () -> Void
    let onNo: () -> Void
    let onTypeInstead: () -> Void
    let onManualRequest: (String) -> Void
    let onManualName: (String) -> Void
    @Published var manualText = ""

    init(
        onStop: @escaping () -> Void,
        onRestart: @escaping () -> Void,
        onYes: @escaping () -> Void,
        onNo: @escaping () -> Void,
        onTypeInstead: @escaping () -> Void,
        onManualRequest: @escaping (String) -> Void,
        onManualName: @escaping (String) -> Void
    ) {
        self.onStop = onStop
        self.onRestart = onRestart
        self.onYes = onYes
        self.onNo = onNo
        self.onTypeInstead = onTypeInstead
        self.onManualRequest = onManualRequest
        self.onManualName = onManualName
    }

    func submitManual() {
        let text = manualText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        if phase == .manualRequest {
            onManualRequest(text)
        } else if phase == .naming {
            onManualName(text)
        }
    }
}

private enum HUDPhase: Equatable {
    case preparing
    case listening
    case transcribing
    case question
    case manualRequest
    case naming
    case building
    case stopping
}

private struct RecordingHUDView: View {
    @ObservedObject var model: HUDModel
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 12) {
            AnyView(statusHeader)
            AnyView(controls)
        }
        .padding(.horizontal, 20)
        .frame(width: 580, height: 220)
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

    private var statusHeader: some View {
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
                    .lineLimit(3)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    @ViewBuilder
    private var controls: some View {
        if model.phase == .manualRequest || model.phase == .naming {
            HStack(spacing: 9) {
                TextField(
                    model.phase == .manualRequest
                        ? "Describe what you want to build"
                        : "Type the exact project name",
                    text: $model.manualText
                )
                .textFieldStyle(.roundedBorder)
                .onSubmit { model.submitManual() }
                Button {
                    model.submitManual()
                } label: {
                    Label("Press Enter", systemImage: "return")
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(
                    model.manualText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                )
            }
        } else if model.phase == .question {
            HStack(spacing: 12) {
                Button(action: model.onNo) {
                    Label("No", systemImage: "xmark").frame(minWidth: 86)
                }
                .buttonStyle(.bordered)
                Button(action: model.onYes) {
                    Label("Yes", systemImage: "checkmark").frame(minWidth: 86)
                }
                .buttonStyle(.borderedProminent)
                Spacer()
                Text("Or answer by voice")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        } else if model.phase == .listening {
            HStack {
                Button(action: model.onTypeInstead) {
                    Label("Manually type what you want instead", systemImage: "keyboard")
                }
                .buttonStyle(.borderedProminent)
                Spacer()
                Text("Or keep speaking")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        } else if model.phase == .transcribing
            || model.phase == .naming
            || model.phase == .building
            || model.phase == .stopping
        {
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
                Text(model.phase == .building
                    ? "Updates every ~15 seconds"
                    : "100% offline")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
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
        case .transcribing: return "text.badge.checkmark"
        case .question: return "questionmark.bubble.fill"
        case .manualRequest: return "keyboard"
        case .naming: return "character.cursor.ibeam"
        case .building: return "hammer.fill"
        case .stopping: return "stop.circle.fill"
        }
    }

    private var title: String {
        switch model.phase {
        case .preparing: return "Starting offline voice"
        case .listening: return "Fractal is listening"
        case .transcribing: return "Improving transcription"
        case .question: return "Confirm with Fractal"
        case .manualRequest: return "Type your build request"
        case .naming: return "Name your project"
        case .building: return "Fractal is building"
        case .stopping: return "Stopping safely"
        }
    }
}
