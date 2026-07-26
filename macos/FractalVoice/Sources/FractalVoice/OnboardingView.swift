import SwiftUI

struct OnboardingView: View {
    @ObservedObject var coordinator: BuildCoordinator
    let finish: () -> Void
    @State private var page = 0

    var body: some View {
        VStack(spacing: 0) {
            TabView(selection: $page) {
                shortcutPage.tag(0)
                examplesPage.tag(1)
                pipelinePage.tag(2)
            }
            .tabViewStyle(.automatic)
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            Divider()
            HStack {
                HStack(spacing: 6) {
                    ForEach(0..<3) { index in
                        Circle()
                            .fill(index == page ? Color.accentColor : Color.secondary.opacity(0.25))
                            .frame(width: 7, height: 7)
                    }
                }
                Spacer()
                if page > 0 {
                    Button("Back") { page -= 1 }
                }
                if page < 2 {
                    Button("Next") { page += 1 }
                        .keyboardShortcut(.defaultAction)
                } else if coordinator.voiceReady {
                    Button("Start using Fractal Voice", action: finish)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Install voice model") {
                        coordinator.installVoiceModel()
                    }
                    .disabled(coordinator.state == .building)
                    .keyboardShortcut(.defaultAction)
                }
            }
            .padding(20)
        }
        .frame(width: 680, height: 500)
        .background(
            LinearGradient(
                colors: [Color(nsColor: .windowBackgroundColor), Color.indigo.opacity(0.08)],
                startPoint: .top,
                endPoint: .bottom
            )
        )
    }

    private var shortcutPage: some View {
        VStack(spacing: 22) {
            Image(systemName: "waveform.circle.fill")
                .font(.system(size: 74))
                .foregroundStyle(.indigo)
            Text("Build with your voice")
                .font(.system(size: 34, weight: .bold, design: .rounded))
            Text("Press once to start. Press again to stop and build.")
                .font(.title3)
                .foregroundStyle(.secondary)
            Text(GlobalHotKey.displayName)
                .font(.system(size: 32, weight: .semibold, design: .rounded))
                .padding(.horizontal, 28)
                .padding(.vertical, 14)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16))
                .overlay(
                    RoundedRectangle(cornerRadius: 16)
                        .stroke(Color.secondary.opacity(0.2))
                )
            Text("Moonshine transcribes locally on your Mac.")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(42)
    }

    private var examplesPage: some View {
        VStack(alignment: .leading, spacing: 24) {
            Label("Say what you want to build", systemImage: "sparkles")
                .font(.system(size: 30, weight: .bold, design: .rounded))
            example("Build a personal expense tracker for iPhone.")
            example("Create a dashboard that monitors my local API.")
            example("Make a simple multiplayer drawing game for the web.")
            Text("Be specific about the platform, core features, and what “done” should look like. Fractal’s lead agent turns your instruction into the PRD and task graph.")
                .font(.body)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(54)
    }

    private var pipelinePage: some View {
        VStack(alignment: .leading, spacing: 22) {
            Label("From speech to working project", systemImage: "point.3.connected.trianglepath.dotted")
                .font(.system(size: 29, weight: .bold, design: .rounded))
            flow("1", "Speak", "Moonshine transcribes on-device.")
            flow("2", "Plan", "The lead agent creates architecture, acceptance criteria, and parallel task waves.")
            flow("3", "Build", "Codex, Cursor, Claude, and available workers execute the live graph.")
            flow("4", "Review", "The graph opens in your browser and refreshes as agents work.")
            HStack(spacing: 10) {
                Image(systemName: coordinator.voiceReady ? "checkmark.seal.fill" : "arrow.down.circle.fill")
                    .foregroundStyle(coordinator.voiceReady ? .green : .indigo)
                Text(coordinator.voiceReady
                    ? "Local voice model is ready."
                    : coordinator.latestActivity)
                    .font(.callout.weight(.medium))
            }
            .padding(12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            Text("Automatic voice builds are limited to fresh, reversible projects under ~/fractal-projects. Destructive and external actions remain blocked.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(48)
    }

    private func example(_ text: String) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "quote.opening")
                .foregroundStyle(.indigo)
            Text(text)
                .font(.title3.weight(.medium))
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
    }

    private func flow(_ number: String, _ title: String, _ detail: String) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Text(number)
                .font(.headline)
                .frame(width: 30, height: 30)
                .background(Color.indigo, in: Circle())
                .foregroundStyle(.white)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(detail).font(.subheadline).foregroundStyle(.secondary)
            }
        }
    }
}
