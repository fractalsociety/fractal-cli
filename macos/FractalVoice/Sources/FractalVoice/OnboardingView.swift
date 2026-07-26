import SwiftUI

struct OnboardingView: View {
    @ObservedObject var coordinator: BuildCoordinator
    let finish: () -> Void
    @StateObject private var readiness = SetupReadiness()
    @State private var page = 0

    private let pageCount = 6

    var body: some View {
        VStack(spacing: 0) {
            Group {
                switch page {
                case 0: accountPage
                case 1: agentPage
                case 2: githubPage
                case 3: readinessPage
                case 4: shortcutPage
                default: buildPage
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            Divider()
            HStack {
                HStack(spacing: 6) {
                    ForEach(0..<pageCount, id: \.self) { index in
                        Circle()
                            .fill(index == page ? Color.accentColor : Color.secondary.opacity(0.25))
                            .frame(width: 7, height: 7)
                    }
                }
                Spacer()
                if page > 0 {
                    Button("Back") { page -= 1 }
                }
                if page < pageCount - 1 {
                    Button(page == 3 && !readiness.isReady ? "Complete setup to continue" : "Next") {
                        page += 1
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(page == 3 && !readiness.isReady)
                } else if readiness.isReady && coordinator.voiceReady {
                    Button("Start using Fractal Voice", action: finish)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button(readiness.isReady ? "Loading offline voice engine…" : "Setup required") {}
                        .disabled(true)
                }
            }
            .padding(20)
        }
        .frame(width: 760, height: 600)
        .background(
            LinearGradient(
                colors: [Color(nsColor: .windowBackgroundColor), Color.indigo.opacity(0.08)],
                startPoint: .top,
                endPoint: .bottom
            )
        )
        .task {
            readiness.refresh()
        }
    }

    private var accountPage: some View {
        VStack(alignment: .leading, spacing: 22) {
            Label("Create your Fractal Society account", systemImage: "person.crop.circle.badge.plus")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            Text("Before your first project, use one email address to create your free Fractal Society account. There is no password to remember.")
                .font(.title3)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            explanation(
                icon: "envelope.badge",
                title: "1. Connect with your email",
                detail: "Choose Connect below. Fractal opens a secure browser page where you enter your email and receive a single-use magic link."
            )
            explanation(
                icon: "checkmark.shield",
                title: "2. Approve Fractal CLI",
                detail: "Open the email link, finish your username if this is a new account, then approve the code shown by Fractal. Return to this app afterward."
            )
            explanation(
                icon: "point.3.connected.trianglepath.dotted",
                title: "3. Confirm the connected account",
                detail: "The setup check verifies the saved CLI session with Fractal Society. Your live execution graphs will publish under that account."
            )

            Button {
                readiness.connectFractalSociety()
            } label: {
                Label(
                    readiness.snapshot.fractalSocietyAuthenticated
                        ? "Fractal Society connected"
                        : readiness.isConnectingSociety ? "Waiting for browser sign-in…" : "Connect Fractal Society account",
                    systemImage: readiness.snapshot.fractalSocietyAuthenticated
                        ? "checkmark.circle.fill"
                        : "safari"
                )
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(readiness.isConnectingSociety || readiness.snapshot.fractalSocietyAuthenticated)

            Text(
                readiness.snapshot.fractalSocietyAuthenticated
                    ? "Connected \(readiness.snapshot.fractalSocietyAccount ?? "account"). You can continue setup."
                    : readiness.societyLoginMessage
                        ?? "If the button cannot open, run `fractal login` in Terminal, complete the email flow, then return and choose Check again."
            )
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(48)
    }

    private var agentPage: some View {
        VStack(alignment: .leading, spacing: 18) {
            Label("Connect an AI builder", systemImage: "cpu")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            Text("Fractal organizes the work, but an AI coding CLI performs it. Install and sign in to at least one supported service before you start.")
                .font(.title3)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            ForEach(SetupReadiness.agentTemplates) { agent in
                HStack(spacing: 14) {
                    Image(systemName: agentIcon(agent.id))
                        .font(.title2)
                        .frame(width: 30)
                        .foregroundStyle(.indigo)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(agent.name).font(.headline)
                        Text("After setup, make sure its CLI is signed in.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Text(agent.command)
                        .font(.system(.caption, design: .monospaced))
                        .padding(7)
                        .background(.quaternary, in: RoundedRectangle(cornerRadius: 7))
                    Link("Setup instructions", destination: agent.setupURL)
                }
                .padding(12)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 13))
            }

            Text("You only need one. Fractal can use additional signed-in agents as parallel workers when they are available.")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(42)
    }

    private var githubPage: some View {
        VStack(alignment: .leading, spacing: 22) {
            Label("Connect Git and GitHub", systemImage: "arrow.triangle.branch")
                .font(.system(size: 31, weight: .bold, design: .rounded))

            explanation(
                icon: "clock.arrow.circlepath",
                title: "Git keeps local history",
                detail: "Fractal commits the source code and .fractal project graph so every change can be reviewed or recovered."
            )
            explanation(
                icon: "cloud",
                title: "GitHub shares and backs up the project",
                detail: "Fractal pushes the repository and execution graph to GitHub, then links that graph to your Fractal Society project page for live progress and collaboration."
            )

            VStack(alignment: .leading, spacing: 10) {
                Text("Sign in from Terminal").font(.headline)
                Text("gh auth login")
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
                Text("Confirm it with:  gh auth status")
                    .font(.system(.callout, design: .monospaced))
                    .foregroundStyle(.secondary)
                Link(
                    "Open GitHub CLI setup instructions",
                    destination: URL(string: "https://docs.github.com/en/github-cli/github-cli/quickstart")!
                )
            }
            .padding(16)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

            Text("Fractal never needs your GitHub password. The official GitHub CLI stores and manages the authorization.")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(48)
    }

    private var readinessPage: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack {
                Label("Setup check", systemImage: "checklist")
                    .font(.system(size: 31, weight: .bold, design: .rounded))
                Spacer()
                Button {
                    readiness.refresh()
                } label: {
                    Label(readiness.isChecking ? "Checking…" : "Check again", systemImage: "arrow.clockwise")
                }
                .disabled(readiness.isChecking)
            }
            Text("Fractal Voice unlocks when Fractal Society, at least one AI CLI, and GitHub are all connected.")
                .font(.title3)
                .foregroundStyle(.secondary)

            VStack(spacing: 0) {
                ForEach(readiness.snapshot.agents) { agent in
                    statusRow(
                        title: agent.name,
                        detail: agent.status,
                        ready: agent.authenticated,
                        link: agent.setupURL
                    )
                    if agent.id != readiness.snapshot.agents.last?.id { Divider() }
                }
            }
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

            VStack(spacing: 0) {
                statusRow(
                    title: "Fractal Society",
                    detail: readiness.snapshot.fractalSocietyAuthenticated
                        ? "Signed in \(readiness.snapshot.fractalSocietyAccount ?? "")"
                        : readiness.snapshot.fractalCLIInstalled ? "Account connection required" : "Fractal CLI missing",
                    ready: readiness.snapshot.fractalSocietyAuthenticated,
                    actionTitle: readiness.snapshot.fractalSocietyAuthenticated ? nil : "Connect",
                    action: readiness.snapshot.fractalCLIInstalled
                        ? { readiness.connectFractalSociety() }
                        : nil
                )
                Divider()
                statusRow(
                    title: "Git",
                    detail: readiness.snapshot.gitInstalled ? "Installed" : "Not installed",
                    ready: readiness.snapshot.gitInstalled
                )
                Divider()
                statusRow(
                    title: "GitHub CLI",
                    detail: readiness.snapshot.githubAuthenticated
                        ? "Signed in"
                        : readiness.snapshot.githubCLIInstalled ? "Sign in required" : "Not installed",
                    ready: readiness.snapshot.githubAuthenticated,
                    link: URL(string: "https://docs.github.com/en/github-cli/github-cli/quickstart")
                )
            }
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

            Label(
                readiness.isReady
                    ? "Setup complete. Fractal Voice is ready to unlock."
                    : readiness.isChecking ? "Checking your command-line setup…" : "Finish the items above, then choose Check again.",
                systemImage: readiness.isReady ? "checkmark.seal.fill" : "lock.fill"
            )
            .font(.callout.weight(.semibold))
            .foregroundStyle(readiness.isReady ? .green : .orange)
        }
        .padding(42)
    }

    private var shortcutPage: some View {
        VStack(spacing: 21) {
            Image(systemName: "waveform.circle.fill")
                .font(.system(size: 62))
                .foregroundStyle(.indigo)
            Text("Your microphone shortcut")
                .font(.system(size: 32, weight: .bold, design: .rounded))
            Text("Press these two keys together from anywhere on your Mac.")
                .font(.title3)
                .foregroundStyle(.secondary)

            HStack(spacing: 16) {
                keycap(symbol: "⌥", label: "option", width: 126)
                Text("+").font(.system(size: 30, weight: .light))
                keycap(symbol: "—", label: "space", width: 230)
            }
            .padding(.vertical, 8)

            VStack(spacing: 7) {
                Text("Press ⌥Space → speak → pause naturally")
                    .font(.headline)
                Text("Fractal keeps the microphone active through the confirmation conversation, then starts the build after your final “yes.”")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Text("The global shortcut does not require Accessibility permission. macOS asks for Microphone permission on your first recording.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: 580)
        }
        .padding(42)
    }

    private var buildPage: some View {
        VStack(alignment: .leading, spacing: 21) {
            Label("Build your first project", systemImage: "sparkles")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            example("Build a personal expense tracker for iPhone.")
            example("Create a dashboard that monitors my local API.")
            example("Make a simple multiplayer drawing game for the web.")
            flow("1", "Speak or type", "Describe the platform, main features, and what done looks like.")
            flow("2", "Confirm", "Fractal confirms spoken input; exact typed input advances when you press Enter.")
            flow("3", "Watch", "The live execution graph opens and shows each agent’s progress.")

            Label(
                coordinator.voiceReady
                    ? "Offline Granite speech and Kokoro voice engines are ready."
                    : coordinator.latestActivity,
                systemImage: coordinator.voiceReady ? "checkmark.seal.fill" : "arrow.down.circle.fill"
            )
            .font(.callout.weight(.medium))
            .foregroundStyle(coordinator.voiceReady ? .green : .indigo)
            .padding(12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        }
        .padding(46)
    }

    private func statusRow(
        title: String,
        detail: String,
        ready: Bool,
        link: URL? = nil,
        actionTitle: String? = nil,
        action: (() -> Void)? = nil
    ) -> some View {
        HStack(spacing: 12) {
            Image(systemName: ready ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                .foregroundStyle(ready ? .green : .orange)
            Text(title).font(.headline)
            Spacer()
            Text(detail).font(.callout).foregroundStyle(.secondary)
            if let link {
                Link("Setup", destination: link)
            }
            if let actionTitle, let action {
                Button(actionTitle, action: action)
                    .disabled(readiness.isConnectingSociety)
            }
        }
        .padding(.horizontal, 15)
        .padding(.vertical, 11)
    }

    private func keycap(symbol: String, label: String, width: CGFloat) -> some View {
        VStack(spacing: 2) {
            Text(symbol)
                .font(.system(size: 28, weight: .medium, design: .rounded))
            Text(label)
                .font(.system(size: 13, weight: .medium, design: .rounded))
        }
        .frame(width: width, height: 82)
        .background(
            LinearGradient(
                colors: [Color.white.opacity(0.25), Color.secondary.opacity(0.12)],
                startPoint: .top,
                endPoint: .bottom
            ),
            in: RoundedRectangle(cornerRadius: 13)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 13)
                .stroke(Color.secondary.opacity(0.45), lineWidth: 2)
        )
        .shadow(color: .black.opacity(0.18), radius: 1, y: 4)
        .accessibilityLabel("\(label) key")
    }

    private func explanation(icon: String, title: String, detail: String) -> some View {
        HStack(alignment: .top, spacing: 15) {
            Image(systemName: icon)
                .font(.title2)
                .foregroundStyle(.indigo)
                .frame(width: 32)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.headline)
                Text(detail).font(.body).foregroundStyle(.secondary)
            }
        }
    }

    private func example(_ text: String) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "quote.opening").foregroundStyle(.indigo)
            Text(text).font(.title3.weight(.medium))
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 13))
    }

    private func flow(_ number: String, _ title: String, _ detail: String) -> some View {
        HStack(alignment: .top, spacing: 13) {
            Text(number)
                .font(.headline)
                .frame(width: 28, height: 28)
                .background(Color.indigo, in: Circle())
                .foregroundStyle(.white)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(detail).font(.subheadline).foregroundStyle(.secondary)
            }
        }
    }

    private func agentIcon(_ id: String) -> String {
        switch id {
        case "codex": return "terminal"
        case "cursor": return "cursorarrow.rays"
        case "claude": return "brain"
        default: return "bolt.horizontal.circle"
        }
    }
}
