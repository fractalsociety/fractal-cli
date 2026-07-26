import Foundation

struct AgentSetup: Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let command: String
    let setupURL: URL
    var installed: Bool
    var authenticated: Bool

    var status: String {
        if authenticated {
            return "Ready"
        }
        return installed ? "Sign in required" : "Not installed"
    }
}

struct SetupSnapshot: Equatable, Sendable {
    var agents: [AgentSetup]
    var gitInstalled: Bool
    var githubCLIInstalled: Bool
    var githubAuthenticated: Bool

    var hasReadyAgent: Bool {
        agents.contains(where: \.authenticated)
    }

    var isReady: Bool {
        hasReadyAgent && gitInstalled && githubCLIInstalled && githubAuthenticated
    }
}

@MainActor
final class SetupReadiness: ObservableObject {
    @Published private(set) var snapshot = SetupReadiness.emptySnapshot
    @Published private(set) var isChecking = false
    @Published private(set) var hasChecked = false

    var isReady: Bool { snapshot.isReady }

    func refresh() {
        guard !isChecking else { return }
        isChecking = true

        DispatchQueue.global(qos: .userInitiated).async {
            let result = Self.checkSystem()
            DispatchQueue.main.async {
                self.snapshot = result
                self.hasChecked = true
                self.isChecking = false
            }
        }
    }

    nonisolated static let agentTemplates: [AgentSetup] = [
        AgentSetup(
            id: "codex",
            name: "OpenAI Codex",
            command: "codex login",
            setupURL: URL(string: "https://learn.chatgpt.com/docs/codex/cli")!,
            installed: false,
            authenticated: false
        ),
        AgentSetup(
            id: "cursor",
            name: "Cursor",
            command: "cursor-agent login",
            setupURL: URL(string: "https://docs.cursor.com/en/cli/installation")!,
            installed: false,
            authenticated: false
        ),
        AgentSetup(
            id: "claude",
            name: "Claude Code",
            command: "claude auth login",
            setupURL: URL(string: "https://docs.anthropic.com/en/docs/claude-code/getting-started")!,
            installed: false,
            authenticated: false
        ),
        AgentSetup(
            id: "hermes",
            name: "Hermes Agent",
            command: "hermes setup",
            setupURL: URL(string: "https://hermes-agent.nousresearch.com/docs/")!,
            installed: false,
            authenticated: false
        )
    ]

    nonisolated static var emptySnapshot: SetupSnapshot {
        SetupSnapshot(
            agents: agentTemplates,
            gitInstalled: false,
            githubCLIInstalled: false,
            githubAuthenticated: false
        )
    }

    nonisolated static func checkSystem() -> SetupSnapshot {
        var agents = agentTemplates
        let checks: [(String, [String])] = [
            ("codex", ["codex", "login", "status"]),
            ("cursor", ["cursor-agent", "status"]),
            ("claude", ["claude", "auth", "status"]),
            ("hermes", ["hermes", "status"])
        ]

        for (id, command) in checks {
            let result = run(command)
            guard let index = agents.firstIndex(where: { $0.id == id }) else {
                continue
            }
            agents[index].installed = result.launched
            agents[index].authenticated = result.launched
                && authenticationSucceeded(for: id, result: result)
        }

        let git = run(["git", "--version"])
        let github = run(["gh", "auth", "status"])
        return SetupSnapshot(
            agents: agents,
            gitInstalled: git.launched && git.exitCode == 0,
            githubCLIInstalled: github.launched,
            githubAuthenticated: github.launched && github.exitCode == 0
        )
    }

    nonisolated static func authenticationSucceeded(
        for id: String,
        result: CommandResult
    ) -> Bool {
        guard result.exitCode == 0 else { return false }
        let output = result.output.lowercased()
        switch id {
        case "codex":
            return output.contains("logged in")
        case "cursor":
            return output.contains("logged in") || output.contains("authenticated")
        case "claude":
            return output.contains("\"loggedin\": true")
                || output.contains("\"loggedin\":true")
        case "hermes":
            return output.contains("logged in")
                || output.contains("authenticated")
                || output.contains("api key") && output.contains("✓")
        default:
            return false
        }
    }

    nonisolated private static func run(_ command: [String]) -> CommandResult {
        guard let executable = command.first else {
            return CommandResult(launched: false, exitCode: -1, output: "")
        }

        let environment = BuildCoordinator.processEnvironment()
        let path = environment["PATH"] ?? ""
        guard let executableURL = findExecutable(executable, path: path) else {
            return CommandResult(launched: false, exitCode: -1, output: "")
        }

        let process = Process()
        let pipe = Pipe()
        process.executableURL = executableURL
        process.arguments = Array(command.dropFirst())
        process.environment = environment
        process.standardOutput = pipe
        process.standardError = pipe
        process.standardInput = FileHandle.nullDevice

        do {
            try process.run()
        } catch {
            return CommandResult(launched: false, exitCode: -1, output: "")
        }

        let deadline = Date().addingTimeInterval(8)
        while process.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        if process.isRunning {
            process.terminate()
            process.waitUntilExit()
        }

        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        return CommandResult(
            launched: true,
            exitCode: process.terminationStatus,
            output: String(decoding: data, as: UTF8.self)
        )
    }

    nonisolated private static func findExecutable(_ name: String, path: String) -> URL? {
        let fileManager = FileManager.default
        for directory in path.split(separator: ":") {
            let candidate = URL(fileURLWithPath: String(directory))
                .appendingPathComponent(name)
            if fileManager.isExecutableFile(atPath: candidate.path) {
                return candidate
            }
        }
        return nil
    }
}

struct CommandResult: Equatable, Sendable {
    let launched: Bool
    let exitCode: Int32
    let output: String
}
