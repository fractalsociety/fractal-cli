import AppKit
import SwiftUI

struct OnboardingView: View {
    @ObservedObject var coordinator: BuildCoordinator
    let finish: () -> Void
    @StateObject private var readiness = SetupReadiness()
    @StateObject private var voiceModels = VoiceModelManager()
    @AppStorage(VoiceInputMode.defaultsKey) private var voiceInputModeRaw = ""
    @AppStorage("selectedLeadAgent") private var selectedLeadAgent = "codex"
    @State private var page: Int
    @State private var bridgeToken = ""
    @State private var showKeychainExplanation = false
    @State private var projectsDirectoryPath = AppRuntime.projectsURL.path
    @State private var projectsDirectoryMessage = ""

    init(
        coordinator: BuildCoordinator,
        initialPage: Int = 0,
        finish: @escaping () -> Void
    ) {
        self.coordinator = coordinator
        self.finish = finish
        _page = State(initialValue: initialPage)
    }

    private var pageCount: Int { AppRuntime.isAppStoreEdition ? 9 : 8 }
    private var readinessPageIndex: Int { AppRuntime.isAppStoreEdition ? 6 : 5 }
    private var accountPageIndex: Int { AppRuntime.isAppStoreEdition ? 2 : 1 }
    private var selectedPlannerReady: Bool {
        readiness.snapshot.agents.first(where: { $0.id == selectedLeadAgent })?.authenticated == true
    }
    private var selectedVoiceMode: VoiceInputMode? {
        VoiceInputMode(rawValue: voiceInputModeRaw)
    }
    private var voiceModeReady: Bool {
        selectedVoiceMode?.isReady(localModelsReady: coordinator.voiceReady) == true
    }
    private var pageBlocksAdvancement: Bool {
        (page == 0 && selectedVoiceMode == nil)
            || (page == readinessPageIndex && (!readiness.isReady || !selectedPlannerReady))
            || (AppRuntime.isAppStoreEdition
                && page == 1
                && !readiness.snapshot.fractalCLIInstalled)
    }

    var body: some View {
        VStack(spacing: 0) {
            currentPage
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
                    Button(
                        pageBlocksAdvancement ? "Complete setup to continue" : "Next"
                    ) {
                        page += 1
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(pageBlocksAdvancement)
                } else if readiness.isReady && voiceModeReady {
                    Button(finishButtonTitle, action: finish)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button(finalBlockedTitle) {}
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
            if selectedVoiceMode == .builtIn {
                voiceModels.startIfNeeded()
            }
        }
        .onChange(of: page) { _, newPage in
            if newPage == accountPageIndex
                || newPage == readinessPageIndex
                || newPage == pageCount - 1 {
                readiness.refresh()
            }
        }
        .onChange(of: voiceModels.isReady) { _, ready in
            if ready, selectedVoiceMode == .builtIn {
                coordinator.activateBuiltInVoice()
            }
        }
        .onChange(of: voiceInputModeRaw) { _, _ in
            configureSelectedVoiceMode()
        }
    }

    @ViewBuilder
    private var currentPage: some View {
        if AppRuntime.isAppStoreEdition {
            switch page {
            case 0: voiceChoicePage
            case 1: bridgePage
            case 2: accountPage
            case 3: agentPage
            case 4: githubPage
            case 5: projectsDirectoryPage
            case 6: readinessPage
            case 7: shortcutPage
            default: buildPage
            }
        } else {
            switch page {
            case 0: voiceChoicePage
            case 1: accountPage
            case 2: agentPage
            case 3: githubPage
            case 4: projectsDirectoryPage
            case 5: readinessPage
            case 6: shortcutPage
            default: buildPage
            }
        }
    }

    private var bridgePage: some View {
        VStack(alignment: .leading, spacing: 22) {
            Label("Connect the sandboxed app to Fractal CLI", systemImage: "cable.connector")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            Text("The App Store edition never receives unrestricted access to your coding agents, Git credentials, or home folder. A local authenticated bridge asks your installed Fractal CLI to perform builds outside the app sandbox.")
                .font(.title3)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            explanation(
                icon: "terminal",
                title: "1. Install the local bridge",
                detail: "Open Terminal and run: fractal bridge install"
            )
            explanation(
                icon: "key.fill",
                title: "2. Copy the pairing token",
                detail: "The command prints a local pairing token. It is stored only on this Mac and should be treated like a local password."
            )
            explanation(
                icon: "lock.shield",
                title: "Why Fractal asks for Keychain access",
                detail: "Fractal saves only this local bridge pairing token in Apple Keychain so another app cannot read it from a settings file. The token authenticates requests to Fractal CLI on 127.0.0.1. Fractal does not read your Apple, GitHub, or coding-agent passwords."
            )

            VStack(alignment: .leading, spacing: 9) {
                Text("Pairing token").font(.headline)
                HStack {
                    SecureField("Paste the token from Terminal", text: $bridgeToken)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit { showKeychainExplanation = true }
                    Button {
                        pasteBridgeToken()
                    } label: {
                        Label("Paste token", systemImage: "doc.on.clipboard")
                    }
                    .help("Paste the entire Fractal bridge token from the clipboard")
                }
                HStack {
                    Button("Save securely and check bridge") {
                        showKeychainExplanation = true
                    }
                    .disabled(bridgeToken.trimmingCharacters(in: .whitespacesAndNewlines).count < 48)
                    if readiness.snapshot.fractalCLIInstalled {
                        Label("Bridge connected", systemImage: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                    }
                    Spacer()
                    if !bridgeToken.isEmpty {
                        Text("\(bridgeToken.trimmingCharacters(in: .whitespacesAndNewlines).count) characters")
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
                if let message = readiness.bridgeMessage {
                    Text(message).font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(16)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

            Link(
                "Open setup and troubleshooting",
                destination: URL(string: "https://fractalsociety.com/support")!
            )
        }
        .padding(44)
        .alert("Allow secure Keychain storage?", isPresented: $showKeychainExplanation) {
            Button("Not now", role: .cancel) {}
            Button("Continue") {
                readiness.pairBridge(token: bridgeToken)
            }
        } message: {
            Text("Fractal Voice will store only the local bridge pairing token in Apple Keychain. It uses the token to prove that build requests sent to Fractal CLI on this Mac are authorized. The token is not uploaded and this does not give Fractal access to your other passwords.")
        }
    }

    private func pasteBridgeToken() {
        guard let clipboard = NSPasteboard.general.string(forType: .string) else {
            readiness.reportBridgeMessage("The clipboard does not contain text.")
            return
        }
        let token = clipboard.trimmingCharacters(in: .whitespacesAndNewlines)
        bridgeToken = token
        showKeychainExplanation = true
    }

    private var voiceChoicePage: some View {
        VStack(alignment: .leading, spacing: 15) {
            Label("Choose how you want to speak to Fractal", systemImage: "waveform")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            Text("ChatGPT Desktop and Superwhisper use their existing voice systems and do not download Fractal’s offline models. You can change this choice later from Show Welcome.")
                .font(.title3)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            voiceOption(
                .chatGPTDesktop,
                icon: "message.fill",
                detail: "Use ChatGPT’s conversational voice. It hands your confirmed request and project name to this app."
            )
            voiceOption(
                .superwhisper,
                icon: "waveform.badge.mic",
                detail: "Use your Superwhisper shortcut and Fractal Command mode through the configured Macrowhisper action."
            )
            voiceOption(
                .builtIn,
                icon: "lock.shield.fill",
                detail: "Private Option–Space conversation using Granite Speech and Kokoro locally. Downloads about 2.5 GB once."
            )

            if let selectedVoiceMode {
                VStack(alignment: .leading, spacing: 8) {
                    switch selectedVoiceMode {
                    case .chatGPTDesktop:
                        HStack {
                            Text("What to say in ChatGPT Desktop or Codex voice").font(.headline)
                            Spacer()
                            Link(
                                "Download ChatGPT",
                                destination: ChatGPTOnboarding.downloadURL
                            )
                        }
                        Text(desktopVoiceInstruction)
                            .font(.system(.callout, design: .rounded).weight(.medium))
                            .textSelection(.enabled)
                        Text("This directs the desktop agent to load Fractal’s operating contract before it uses the secure native handoff. Keep Fractal Voice running in the menu bar.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    case .superwhisper:
                        Text("What to say in Superwhisper").font(.headline)
                        Text("“Build [describe the project] and call it [your project name].”")
                            .font(.system(.callout, design: .rounded).weight(.medium))
                            .textSelection(.enabled)
                        Text("Select your Fractal Command mode and make sure Macrowhisper’s completed-transcript action is configured. Fractal does not download its local voice models for this option.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    case .builtIn:
                        HStack {
                            Text(voiceModelStatus).font(.headline)
                            Spacer()
                            Text(voiceModelProgress)
                                .font(.callout.monospacedDigit())
                                .foregroundStyle(.secondary)
                        }
                        ProgressView(value: voiceModels.progress)
                            .progressViewStyle(.linear)
                        Text(voiceModels.currentFile)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        if case .failed(let message) = voiceModels.state {
                            Text(message).font(.callout).foregroundStyle(.red)
                            Button("Retry download") { voiceModels.retry() }
                        }
                    }
                }
                .padding(13)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 13))
            }
        }
        .padding(36)
    }

    private func voiceOption(
        _ mode: VoiceInputMode,
        icon: String,
        detail: String
    ) -> some View {
        Button {
            VoiceInputMode.save(mode)
            voiceInputModeRaw = mode.rawValue
        } label: {
            HStack(spacing: 13) {
                Image(systemName: icon)
                    .font(.title2)
                    .frame(width: 34)
                VStack(alignment: .leading, spacing: 3) {
                    Text(mode.title).font(.headline)
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.leading)
                }
                Spacer()
                Image(systemName: selectedVoiceMode == mode
                    ? "checkmark.circle.fill"
                    : "circle")
                    .foregroundStyle(selectedVoiceMode == mode ? .green : .secondary)
            }
            .padding(11)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            selectedVoiceMode == mode ? Color.indigo.opacity(0.12) : Color.clear,
            in: RoundedRectangle(cornerRadius: 13)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 13)
                .stroke(
                    selectedVoiceMode == mode
                        ? Color.indigo.opacity(0.7)
                        : Color.secondary.opacity(0.22)
                )
        )
    }

    private func configureSelectedVoiceMode() {
        guard let selectedVoiceMode else { return }
        if selectedVoiceMode == .builtIn {
            voiceModels.startIfNeeded()
            if voiceModels.isReady {
                coordinator.activateBuiltInVoice()
            }
        } else {
            coordinator.activateExternalVoice(selectedVoiceMode)
        }
    }

    private var finishButtonTitle: String {
        switch selectedVoiceMode {
        case .chatGPTDesktop: return "Start with ChatGPT Desktop"
        case .superwhisper: return "Start with Superwhisper"
        case .builtIn: return "Start using built-in voice"
        case nil: return "Choose a voice option"
        }
    }

    private var finalBlockedTitle: String {
        if !readiness.isReady { return "Setup required" }
        if selectedVoiceMode == nil { return "Choose a voice option" }
        return "Loading offline voice engine…"
    }

    private var voiceModelStatus: String {
        switch voiceModels.state {
        case .checking: return "Checking installed models…"
        case .downloading: return "Installing offline voice models"
        case .ready: return "Offline voice engine ready"
        case .failed: return "Download needs attention"
        }
    }

    private var voiceModelProgress: String {
        ByteCountFormatter.string(
            fromByteCount: voiceModels.downloadedBytes,
            countStyle: .file
        ) + " / " + ByteCountFormatter.string(
            fromByteCount: voiceModels.totalBytes,
            countStyle: .file
        )
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
                        ? "Connected \(readiness.snapshot.fractalSocietyAccount ?? "to Fractal Society")"
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
                    ? "Your account is verified. You can continue setup."
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

            HStack(spacing: 14) {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Main planner").font(.headline)
                    Text("Choose your smartest available agent. It designs the PRD, architecture, task graph, and reviews mid-build changes.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Picker("Main planner", selection: $selectedLeadAgent) {
                    ForEach(readiness.snapshot.agents) { agent in
                        Text("\(agent.name)\(agent.authenticated ? " — Ready" : " — setup required")")
                            .tag(agent.id)
                    }
                }
                .labelsHidden()
                .frame(width: 170)
            }
            .padding(12)
            .background(Color.indigo.opacity(0.1), in: RoundedRectangle(cornerRadius: 13))

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

    private var projectsDirectoryPage: some View {
        VStack(alignment: .leading, spacing: 22) {
            Label("Choose where Fractal saves projects", systemImage: "folder.badge.gearshape")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            Text("Every voice or desktop build gets its own folder here. Fractal keeps the source code, Git repository, and portable execution graph together.")
                .font(.title3)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 10) {
                Text("Current project location").font(.headline)
                Text(projectsDirectoryPath)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .lineLimit(2)
                    .truncationMode(.middle)
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 9))
                HStack {
                    Button {
                        chooseProjectsDirectory()
                    } label: {
                        Label("Choose a different folder…", systemImage: "folder")
                    }
                    Button("Use default") {
                        useDefaultProjectsDirectory()
                    }
                    Spacer()
                    Button("Open folder") {
                        coordinator.openProjects()
                    }
                }
                if !projectsDirectoryMessage.isEmpty {
                    Text(projectsDirectoryMessage)
                        .font(.caption)
                        .foregroundStyle(
                            projectsDirectoryMessage.hasPrefix("Could not") ? .red : .secondary
                        )
                }
            }
            .padding(16)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

            explanation(
                icon: "doc.text.fill",
                title: "Global agent instructions are included",
                detail: "Fractal creates AGENTS.md in this folder so ChatGPT Desktop, Codex, and other compatible agents can discover how to hand builds to the Fractal orchestrator."
            )
            explanation(
                icon: "folder.fill.badge.plus",
                title: "Each project remains self-contained",
                detail: "A project-level AGENTS.md and .fractal folder are also created inside every new project. Changing this setting affects new builds; existing projects are not moved or deleted."
            )
        }
        .padding(44)
    }

    private func chooseProjectsDirectory() {
        let panel = NSOpenPanel()
        panel.title = "Choose the Fractal projects folder"
        panel.prompt = "Use This Folder"
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.directoryURL = AppRuntime.projectsURL
        guard panel.runModal() == .OK, let selected = panel.url else { return }
        do {
            try AppRuntime.configureProjectsURL(selected)
            projectsDirectoryPath = AppRuntime.projectsURL.path
            projectsDirectoryMessage = "New projects will be saved here. AGENTS.md is ready."
        } catch {
            projectsDirectoryMessage = "Could not use this folder: \(error.localizedDescription)"
        }
    }

    private func useDefaultProjectsDirectory() {
        do {
            try AppRuntime.useDefaultProjectsURL()
            projectsDirectoryPath = AppRuntime.projectsURL.path
            projectsDirectoryMessage = "Restored the default project location."
        } catch {
            projectsDirectoryMessage = "Could not restore the default: \(error.localizedDescription)"
        }
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

            if !selectedPlannerReady {
                Label(
                    "Your selected main planner must be installed and signed in.",
                    systemImage: "brain.head.profile"
                )
                .font(.callout.weight(.semibold))
                .foregroundStyle(.orange)
            }

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
        Group {
            switch selectedVoiceMode {
            case .chatGPTDesktop:
                externalVoiceInstructionPage(
                    icon: "message.fill",
                    title: "Build through ChatGPT Desktop or Codex voice",
                    steps: [
                        "Keep Fractal Voice running in your menu bar.",
                        "Open ChatGPT Desktop and start a voice conversation.",
                        "Say: \(desktopVoiceExample)",
                        "ChatGPT confirms the details and sends the named build through Fractal’s secure handoff.",
                    ],
                    footer: "The instruction file tells the desktop agent to use `fractal handoff`, not the deprecated bridge. If it reports “Queued,” Fractal Voice will pick up the accepted request automatically."
                )
            case .superwhisper:
                externalVoiceInstructionPage(
                    icon: "waveform.badge.mic",
                    title: "Build through Superwhisper",
                    steps: [
                        "Open Superwhisper and select your Fractal Command mode.",
                        "Make sure Macrowhisper sends completed transcripts to your Fractal action.",
                        "Use your Superwhisper shortcut and say: “Build a personal expense tracker and call it Pocket Ledger.”",
                        "Stop recording normally; the completed transcript enters Fractal’s build workflow.",
                    ],
                    footer: "Superwhisper handles transcription, so Fractal’s 2.5 GB offline voice download is not required."
                )
            case .builtIn:
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
                        Text("The shortcut does not require Accessibility permission. macOS asks for Microphone permission on your first recording.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                    }
                    .frame(maxWidth: 580)
                }
                .padding(42)
            case nil:
                Text("Return to the first page and choose a voice option.")
            }
        }
    }

    @ViewBuilder
    private var buildPage: some View {
        if selectedVoiceMode == .chatGPTDesktop {
            chatGPTFinalPage
        } else {
            standardBuildPage
        }
    }

    private var standardBuildPage: some View {
        VStack(alignment: .leading, spacing: 21) {
            Label("Build your first project", systemImage: "sparkles")
                .font(.system(size: 31, weight: .bold, design: .rounded))
            example("Build a personal expense tracker for iPhone.")
            example("Create a dashboard that monitors my local API.")
            example("Make a simple multiplayer drawing game for the web.")
            flow("1", "Describe and name it", finalBuildInputDescription)
            flow("2", "Hand off", finalBuildHandoffDescription)
            flow("3", "Watch", "The live execution graph opens and shows each agent’s progress.")

            Label(
                finalVoiceStatus,
                systemImage: voiceModeReady ? "checkmark.seal.fill" : "arrow.down.circle.fill"
            )
            .font(.callout.weight(.medium))
            .foregroundStyle(voiceModeReady ? .green : .indigo)
            .padding(12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        }
        .padding(46)
    }

    private var chatGPTFinalPage: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 17) {
                HStack {
                    Label("Start with ChatGPT Desktop voice", systemImage: "message.fill")
                        .font(.system(size: 29, weight: .bold, design: .rounded))
                    Spacer()
                    Link("Download ChatGPT for macOS", destination: ChatGPTOnboarding.downloadURL)
                }

                HStack(alignment: .top, spacing: 20) {
                    chatGPTVoiceIcon
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Open ChatGPT, sign in, then click this voice button")
                            .font(.headline)
                        Text("The white waveform button starts voice mode. Keep Fractal Voice running in your menu bar so it can receive secure build and sharing handoffs.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(14)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

                VStack(alignment: .leading, spacing: 8) {
                    Label("Choose how commands are approved", systemImage: "lock.shield")
                        .font(.headline)
                    Text("Hands-free: open ChatGPT Settings → General → Permissions, enable Full access, then select Full access beneath the composer. This lets ChatGPT access files and run networked commands without asking, so use it only when you trust the task.")
                        .font(.callout)
                    Text("Manual review: leave Ask for approval selected. ChatGPT pauses before external commands, so check back and approve each request yourself.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    Link("Read OpenAI’s permission guide", destination: ChatGPTOnboarding.permissionsURL)
                        .font(.caption)
                }
                .padding(14)
                .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 14))

                VStack(alignment: .leading, spacing: 7) {
                    Text("Say this once at the start of the voice conversation")
                        .font(.headline)
                    Text(desktopVoiceBootstrapInstruction)
                        .font(.system(.callout, design: .rounded).weight(.medium))
                        .textSelection(.enabled)
                    Text("This points the agent to the Fractal-owned project folder in your user directory and teaches it the supported handoff commands.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(14)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))

                Text("What you can ask by voice").font(.headline)
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 9) {
                    voiceCapability("hammer.fill", "Build and name a project")
                    voiceCapability("pause.circle.fill", "Pause a named project")
                    voiceCapability("person.crop.circle.badge.plus", "Share or invite project help")
                    voiceCapability("list.number", "Explain any execution-graph task")
                }
            }
            .padding(36)
        }
    }

    @ViewBuilder
    private var chatGPTVoiceIcon: some View {
        if let imageURL = ChatGPTOnboarding.voiceIconURL,
           let image = NSImage(contentsOf: imageURL) {
            Image(nsImage: image)
                .resizable()
                .scaledToFit()
                .frame(width: 82, height: 82)
                .clipShape(RoundedRectangle(cornerRadius: 18))
                .overlay(
                    RoundedRectangle(cornerRadius: 18)
                        .stroke(Color.secondary.opacity(0.25))
                )
                .accessibilityLabel("ChatGPT voice mode waveform button")
        } else {
            Image(systemName: "waveform.circle.fill")
                .font(.system(size: 72))
                .accessibilityLabel("ChatGPT voice mode waveform button")
        }
    }

    private func voiceCapability(_ icon: String, _ title: String) -> some View {
        Label(title, systemImage: icon)
            .font(.callout.weight(.medium))
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.indigo.opacity(0.08), in: RoundedRectangle(cornerRadius: 11))
    }

    private func externalVoiceInstructionPage(
        icon: String,
        title: String,
        steps: [String],
        footer: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 20) {
            Label(title, systemImage: icon)
                .font(.system(size: 31, weight: .bold, design: .rounded))
            ForEach(Array(steps.enumerated()), id: \.offset) { index, step in
                flow(String(index + 1), index == 2 ? "What to say" : "Step \(index + 1)", step)
            }
            Text(footer)
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(14)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 13))
            Button("Choose a different voice option") { page = 0 }
        }
        .padding(46)
    }

    private var finalBuildInputDescription: String {
        switch selectedVoiceMode {
        case .chatGPTDesktop:
            return "Tell ChatGPT what to build and the exact project name in the same voice conversation."
        case .superwhisper:
            return "Speak the build description and project name in your Fractal Command mode."
        case .builtIn:
            return "Press ⌥Space, describe the project, and answer Fractal’s confirmation questions."
        case nil:
            return "Choose a voice option first."
        }
    }

    private var desktopAgentInstructionsPath: String {
        AppRuntime.projectsURL.appendingPathComponent("AGENTS.md").path
    }

    private var desktopVoiceBootstrapInstruction: String {
        "“Look in my user folder for the Fractal projects folder at \(AppRuntime.projectsURL.path). Read \(desktopAgentInstructionsPath) and follow its External desktop app instructions for every Fractal request.”"
    }

    private var desktopVoiceInstruction: String {
        "\(desktopVoiceBootstrapInstruction) Then say: “Use Fractal to build [describe the project]. Name the project [your project name].”"
    }

    private var desktopVoiceExample: String {
        "“First read \(desktopAgentInstructionsPath) and follow its External desktop app instructions. Then use Fractal to build a personal expense tracker. Name the project Pocket Ledger.”"
    }

    private var finalBuildHandoffDescription: String {
        switch selectedVoiceMode {
        case .chatGPTDesktop:
            return "ChatGPT calls `fractal handoff`; Fractal Voice receives sent or securely queued requests."
        case .superwhisper:
            return "Macrowhisper passes the completed transcript into Fractal."
        case .builtIn:
            return "Fractal starts after you confirm the request and project name."
        case nil:
            return "Voice setup is incomplete."
        }
    }

    private var finalVoiceStatus: String {
        switch selectedVoiceMode {
        case .chatGPTDesktop:
            return "ChatGPT Desktop handoff is ready. No Fractal voice models were downloaded."
        case .superwhisper:
            return "Superwhisper mode is selected. No Fractal voice models were downloaded."
        case .builtIn:
            return coordinator.voiceReady
                ? "Offline Granite speech and Kokoro voice engines are ready."
                : "Voice models are still installing — return to the first page for progress."
        case nil:
            return "Choose a voice option on the first page."
        }
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

enum ChatGPTOnboarding {
    static let downloadURL = URL(string: "https://chatgpt.com/download/")!
    static let permissionsURL = URL(
        string: "https://learn.chatgpt.com/docs/permission-modes#enable-modes"
    )!
    static var voiceIconURL: URL? {
        Bundle.module.url(forResource: "ChatGPTVoiceIcon", withExtension: "png")
    }
}
