use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const DEFAULT_BOARD_URL: &str = "http://127.0.0.1:8091";

/// Default port for a per-graph board (shared by `submit` auto-open and
/// `graph board`).
pub(crate) const DEFAULT_GRAPH_PORT: u16 = 8092;

/// Command-line arguments for the Fractal pipeline front door.
#[derive(Debug, Parser)]
#[command(
    name = "fractal",
    version,
    disable_version_flag = true,
    about = "Run `fractal` with no arguments to start an interactive session, or use a subcommand"
)]
pub(crate) struct Cli {
    /// FractalWork checkout containing the TypeScript intent classifier.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) fractalwork: Option<PathBuf>,

    /// Use the Coordinate backend: reconcile each graph into the real durable
    /// Coordinate pull-queue (needs `squad`) instead of the in-process executor.
    /// Also settable per session with `$FRACTAL_BACKEND=coordinate` or `/backend`.
    #[arg(long, global = true)]
    pub(crate) coordinate: bool,

    /// Start a local-only interactive session without Fractal Society login.
    #[arg(long, global = true)]
    pub(crate) offline: bool,

    /// A natural-language request to submit with default options.
    #[arg(value_name = "REQUEST")]
    pub(crate) request: Option<String>,

    /// An explicit Fractal operation.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Top-level CLI operations.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Parse a request and preview its intended pipeline stages.
    Submit(SubmitArgs),
    /// Build, verify, and optionally launch a native SwiftUI iOS app.
    Ios(IosArgs),
    /// Build a cross-platform mobile app (Expo by default).
    Mobile(MobileArgs),
    /// Normalize stdin into fractal.input.v1 and route it through the safety gate.
    Ingest(IngestArgs),
    /// Record a command with native Moonshine (default) or Superwhisper.
    Voice(VoiceArgs),
    /// Record dictation with native Moonshine (default) or Superwhisper.
    Dictate(VoiceArgs),
    /// Open or inspect the live execution-graph board.
    Graph(GraphArgs),
    /// Validate or inspect the repository harness policy without writing.
    Harness(HarnessArgs),
    /// Record, inspect, or revoke external human-review gates.
    Gate(GateArgs),
    /// Inspect or govern queued graph amendments.
    Amendment(AmendmentArgs),
    /// Run a compiled graph through Coordinate (stub).
    Run(RunArgs),
    /// Run the graph morphogenesis loop (stub).
    Evolve(EvolveArgs),
    /// Inspect or configure governed execution-efficiency controls.
    Efficiency(EfficiencyArgs),
    /// Inspect or control one graph node (stub).
    Node(NodeArgs),
    /// Join the local project as a worker and wait for coordinator assignment.
    Join(JoinArgs),
    /// Run the local project coordinator assignment loop.
    Coordinator(CoordinatorArgs),
    /// Continuously form one-leader/five-worker specialist teams.
    Architect(ArchitectArgs),
    /// Safely clear a fractal workspace/test folder (guarded + confirmed).
    Clean(CleanArgs),
    /// GRPO-train an adapter from accumulated verifiable rewards (fractal-rlvr).
    Train,
    /// Show + verify the durable machine-scale chain of folded run receipts.
    Chain,
    /// List numbered projects and their resume status.
    Projects,
    /// Resume a project by its number (also: say "resume project N" by voice).
    Resume(ResumeArgs),
    /// Stop the active project, a named project, or every running build.
    #[command(visible_alias = "pause")]
    Stop(StopArgs),
    /// Inspect live Fractal build processes.
    Status(StatusArgs),
    /// Log in through Fractal Society in the browser.
    Login(LoginArgs),
    /// Remove the locally stored Fractal Society session.
    Logout,
    /// Publish this project's standardized graph (explicitly or opt-in).
    Sync(SyncArgs),
    /// Hand a named managed build to the native Fractal Voice app.
    Handoff(HandoffArgs),
    /// Accept a secure website task handoff and work on its review branch.
    Contribute(ContributeArgs),
    /// Email a secure project invitation after explicit confirmation.
    Invite(InviteArgs),
    /// Ask an X user for help through Fractal Voice and X's prefilled composer.
    ShareX(ShareXArgs),
    /// Deprecated no-op: X OAuth is disabled; use `share-x`.
    ConnectX(ConnectXArgs),
    /// Preview or confirm a project and GitHub repository visibility change.
    Visibility(VisibilityArgs),
    /// Deprecated compatibility parser. Use `handoff`; this command is hidden
    /// and never starts the removed loopback bridge.
    #[command(hide = true)]
    Bridge(BridgeArgs),
    /// Print the Fractal CLI version.
    Version,
}

/// Arguments accepted by `fractal amendment`.
#[derive(Debug, Args)]
pub(crate) struct AmendmentArgs {
    /// Amendment control-plane operation.
    #[command(subcommand)]
    pub(crate) command: AmendmentCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AmendmentCommand {
    /// List every pending graph amendment without changing the queue.
    List(AmendmentListArgs),
    /// Reject one pending graph amendment. Without `--yes`, print a read-only
    /// preview that can be reviewed before the exact target is removed.
    Reject(AmendmentRejectArgs),
}

#[derive(Debug, Args)]
pub(crate) struct AmendmentListArgs {
    /// Project workspace containing the pending amendment queue.
    #[arg(long, required = true, value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AmendmentRejectArgs {
    /// Exact pending amendment command ID to reject.
    #[arg(value_name = "COMMAND_ID")]
    pub(crate) command_id: String,
    /// Project workspace containing the pending amendment queue.
    #[arg(long, required = true, value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Human-readable reason recorded in the owner-only rejection audit.
    #[arg(long, required = true, value_name = "TEXT")]
    pub(crate) reason: String,
    /// Apply the rejection. Without this flag the command is read-only.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments accepted by `fractal harness`.
#[derive(Debug, Args)]
pub(crate) struct HarnessArgs {
    /// Harness policy operation.
    #[command(subcommand)]
    pub(crate) command: HarnessCommand,
}

/// Arguments accepted by fractal gate.
#[derive(Debug, Args)]
pub(crate) struct GateArgs {
    #[command(subcommand)]
    pub(crate) command: GateCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum GateCommand {
    /// Preview or append one immutable external-gate approval.
    Record(GateRecordArgs),
    /// Show the append-only external-gate ledger.
    Show(GateShowArgs),
    /// Preview or append one exact approval revocation.
    Revoke(GateRevokeArgs),
}

#[derive(Debug, Args)]
pub(crate) struct GateRecordArgs {
    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Graph node that declares the external gate.
    #[arg(long, visible_alias = "node-id", value_name = "NODE")]
    pub(crate) node: String,
    /// Exact declared gate name (for example security_review).
    #[arg(long, value_name = "GATE")]
    pub(crate) gate: String,
    /// Repo-relative evidence file.
    #[arg(long, visible_alias = "evidence-path", value_name = "PATH")]
    pub(crate) evidence: PathBuf,
    /// Local reviewer identity; never taken from a worker checkout.
    #[arg(long, value_name = "ID")]
    pub(crate) reviewer_id: String,
    /// Human-readable reviewer label.
    #[arg(long, default_value = "", value_name = "LABEL")]
    pub(crate) reviewer_label: String,
    /// Bounded reviewer role (security_review requires security_reviewer).
    #[arg(long, value_name = "ROLE")]
    pub(crate) role: String,
    /// Local attestation text or reference.
    #[arg(long, value_name = "TEXT")]
    pub(crate) attestation: String,
    /// Apply the exact previewed record. Without this flag the command is read-only.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Content hash printed by the preview; required with --yes to detect
    /// document, ledger, or evidence drift between commands.
    #[arg(long, visible_alias = "expected-hash", value_name = "HASH")]
    pub(crate) expected_content_hash: Option<String>,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct GateShowArgs {
    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Restrict output to one node.
    #[arg(long, visible_alias = "node-id", value_name = "NODE")]
    pub(crate) node: Option<String>,
    /// Restrict output to one gate.
    #[arg(long, value_name = "GATE")]
    pub(crate) gate: Option<String>,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct GateRevokeArgs {
    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Exact content hash printed by gate record/show.
    #[arg(long, visible_alias = "record-hash", value_name = "HASH")]
    pub(crate) approval_hash: String,
    /// Local revoker identity.
    #[arg(long, value_name = "ID")]
    pub(crate) reviewer_id: String,
    /// Human-readable revoker label.
    #[arg(long, default_value = "", value_name = "LABEL")]
    pub(crate) reviewer_label: String,
    /// Bounded revoker role.
    #[arg(long, value_name = "ROLE")]
    pub(crate) role: String,
    /// Local revocation attestation text or reference.
    #[arg(long, value_name = "TEXT")]
    pub(crate) attestation: String,
    /// Apply the exact previewed revocation. Without this flag it is read-only.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Content hash printed by the preview; required with --yes to detect
    /// approval, project, or ledger drift. Old approval evidence may drift or
    /// be deleted without blocking an exact revocation.
    #[arg(long, visible_alias = "expected-hash", value_name = "HASH")]
    pub(crate) expected_content_hash: Option<String>,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum HarnessCommand {
    /// Parse, normalize, validate, and hash the repository policy.
    Validate(HarnessPolicyArgs),
    /// Show the normalized policy, provenance, and canonical hash.
    Show(HarnessPolicyArgs),
}

#[derive(Debug, Args)]
pub(crate) struct HarnessPolicyArgs {
    /// Project workspace containing `.fractal/harness.yaml` or JSON.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,
    /// Print a stable JSON report.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct HandoffArgs {
    /// Confirmed project name used for its folder, graph title, and profile URL.
    #[arg(long = "name", visible_alias = "project-name", value_name = "NAME")]
    pub(crate) project_name: String,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ContributeArgs {
    /// Short-lived, single-use task handoff issued by Fractal Society.
    #[arg(long, value_name = "TOKEN")]
    pub(crate) token: String,

    /// Fractal Society origin that issued the handoff.
    #[arg(long, value_name = "URL")]
    pub(crate) server: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum InvitationRole {
    Maintainer,
    Contributor,
    Viewer,
}

impl InvitationRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Maintainer => "maintainer",
            Self::Contributor => "contributor",
            Self::Viewer => "viewer",
        }
    }
}

#[derive(Debug, clap::Args)]
pub(crate) struct InviteArgs {
    /// Fractal Society project slug.
    #[arg(long, value_name = "SLUG")]
    pub(crate) project: String,
    /// Recipient email address.
    #[arg(long, value_name = "EMAIL")]
    pub(crate) email: String,
    /// Access granted by the invitation.
    #[arg(long, value_enum, default_value_t = InvitationRole::Contributor)]
    pub(crate) role: InvitationRole,
    /// Plain-language description of the help or compute requested.
    #[arg(long = "message", visible_alias = "help-request", value_name = "TEXT")]
    pub(crate) message: Option<String>,
    /// Confirm the external email side effect.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Fractal Society origin (defaults to the saved login server).
    #[arg(long, value_name = "URL")]
    pub(crate) server: Option<String>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ShareXArgs {
    /// Fractal Society project slug.
    #[arg(long, value_name = "SLUG")]
    pub(crate) project: String,
    /// X handle to mention.
    #[arg(long, value_name = "HANDLE")]
    pub(crate) handle: String,
    /// Plain-language description of the help or compute requested.
    #[arg(long = "message", visible_alias = "help-request", value_name = "TEXT")]
    pub(crate) message: Option<String>,
    /// Send the displayed preview to Fractal Voice, which opens X's composer.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Print the exact post without opening the website preview (for desktop agents).
    #[arg(long, conflicts_with = "yes")]
    pub(crate) preview_only: bool,
    /// Fractal Society origin (defaults to the saved login server).
    #[arg(long, value_name = "URL")]
    pub(crate) server: Option<String>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ConnectXArgs {
    /// Ignored legacy project option.
    #[arg(long, value_name = "SLUG")]
    pub(crate) project: Option<String>,
    /// Ignored legacy server option.
    #[arg(long, value_name = "URL")]
    pub(crate) server: Option<String>,
    /// Ignored legacy option.
    #[arg(long)]
    pub(crate) no_open: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct VisibilityArgs {
    /// Project slug, folder name, or registered workspace path.
    #[arg(long, value_name = "NAME")]
    pub(crate) project: String,
    /// Make the Fractal Society graph and linked GitHub repository public.
    #[arg(long, conflicts_with = "private", required_unless_present = "private")]
    pub(crate) public: bool,
    /// Make the Fractal Society graph and linked GitHub repository private.
    #[arg(long, conflicts_with = "public", required_unless_present = "public")]
    pub(crate) private: bool,
    /// Confirm the warned visibility change.
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct BridgeArgs {
    #[command(subcommand)]
    pub(crate) command: Option<BridgeCommand>,
}

/// Stable migration output for scripts that still invoke the removed bridge
/// command. Keep the parser above so old invocations receive this message
/// instead of an opaque unknown-command error, but never dispatch a bridge
/// server, launch agent, or pairing-token operation.
pub(crate) const BRIDGE_MIGRATION_MESSAGE: &str =
    "`fractal bridge` is no longer available. Use `fractal handoff --name 'PROJECT NAME'` and pass the build request on stdin.";

#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum BridgeCommand {
    /// Run the loopback-only bridge in the foreground.
    #[command(hide = true)]
    Serve {
        #[arg(long, default_value_t = 18_372)]
        port: u16,
    },
    /// Install and start the per-user launch agent.
    #[command(hide = true)]
    Install {
        #[arg(long, default_value_t = 18_372)]
        port: u16,
    },
    /// Print the pairing token for entry into Fractal Voice.
    #[command(hide = true)]
    Token,
    /// Verify that the local bridge is reachable.
    #[command(hide = true)]
    Status {
        #[arg(long, default_value_t = 18_372)]
        port: u16,
    },
}

#[derive(Debug, clap::Args)]
pub(crate) struct LoginArgs {
    /// Fractal Society origin (defaults to https://fractalsociety.com).
    #[arg(long, value_name = "URL")]
    pub(crate) server: Option<String>,

    /// Print the authorization URL instead of opening a browser.
    #[arg(long)]
    pub(crate) no_open: bool,

    /// Maximum seconds to wait for browser authorization.
    #[arg(long, default_value_t = 300)]
    pub(crate) timeout: u64,

    /// Verify the saved Fractal Society session without starting a login.
    #[arg(long)]
    pub(crate) status: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct SyncArgs {
    /// Project workspace (defaults to the current directory).
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Enable future profile and local-GitHub sync.
    #[arg(long, conflicts_with = "disable")]
    pub(crate) enable: bool,

    /// Opt this project out of automatic web uploads.
    #[arg(long, conflicts_with_all = ["enable", "github"])]
    pub(crate) disable: bool,

    /// Require the local GitHub push to succeed instead of treating it as fail-soft.
    #[arg(long)]
    pub(crate) github: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct StopArgs {
    /// Stop a running project by folder name, slug, or absolute workspace path.
    #[arg(long, value_name = "NAME", conflicts_with = "all")]
    pub(crate) project: Option<String>,

    /// Stop every running Fractal build.
    #[arg(long)]
    pub(crate) all: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct StatusArgs {
    /// Show only builds that are currently running.
    #[arg(long)]
    pub(crate) running: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ResumeArgs {
    /// Project number, as shown by `fractal projects`.
    pub(crate) number: u32,

    /// Board port to serve the resumed graph on.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,
}

/// Arguments accepted by `fractal clean`.
#[derive(Debug, Args)]
pub(crate) struct CleanArgs {
    /// Directory to clear — must resolve inside a fractal disposable folder
    /// (`$FRACTAL_HOME`/`~/.fractal`, `~/fractal-test`, `~/fractal-demo`,
    /// `~/fractal-runs`). Any other path is refused.
    #[arg(value_name = "DIR")]
    pub(crate) dir: PathBuf,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
}

/// Arguments accepted by `fractal submit`.
#[derive(Debug, Args)]
pub(crate) struct SubmitArgs {
    /// The natural-language work request.
    pub(crate) request: String,

    /// Stop after planning or continue toward a build.
    #[arg(long, value_enum)]
    pub(crate) mode: Option<Mode>,

    /// Preferred worker provider.
    #[arg(long, value_enum)]
    pub(crate) provider: Option<Provider>,

    /// Repository in which the future pipeline should operate.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Port for the auto-opened per-graph board (build mode).
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,

    /// Do not auto-open the execution-graph viewer after a build.
    #[arg(long)]
    pub(crate) no_open: bool,
}

/// Arguments accepted by `fractal ios`.
#[derive(Debug, Args)]
pub(crate) struct IosArgs {
    /// Natural-language description of the app to build.
    pub(crate) request: String,

    /// Build, install, and open the app in iOS Simulator after verification.
    #[arg(long)]
    pub(crate) launch: bool,

    /// Project folder. By default Fractal creates one under `~/fractal-projects`.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Simulator device to use.
    #[arg(long, default_value = "iPhone 17 Pro")]
    pub(crate) simulator: String,

    /// Print the specialized plan without writing or running anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// Arguments accepted by `fractal mobile`.
#[derive(Debug, Args)]
pub(crate) struct MobileArgs {
    /// Natural-language description of the app to build.
    pub(crate) request: String,

    /// Cross-platform framework.
    #[arg(long, value_enum, default_value_t = MobileFramework::Expo)]
    pub(crate) framework: MobileFramework,

    /// Target platforms.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "ios,android")]
    pub(crate) platforms: Vec<MobilePlatform>,

    /// Platform to launch after verification.
    #[arg(long, value_enum)]
    pub(crate) launch: Option<MobilePlatform>,

    /// Project folder. By default Fractal creates one under `~/fractal-projects`.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// iOS Simulator device to use when launching iOS.
    #[arg(long, default_value = "iPhone 17 Pro")]
    pub(crate) simulator: String,

    /// Print the specialized plan without writing or running anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MobileFramework {
    Expo,
}

impl std::fmt::Display for MobileFramework {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Expo => "expo",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MobilePlatform {
    Ios,
    Android,
}

impl std::fmt::Display for MobilePlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Ios => "ios",
            Self::Android => "android",
        })
    }
}

/// Arguments accepted by `fractal ingest`.
#[derive(Debug, Args)]
pub(crate) struct IngestArgs {
    /// Source adapter that produced the input event.
    #[arg(long, default_value = "terminal")]
    pub(crate) source: String,

    /// Encoding read from stdin.
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub(crate) format: InputFormat,

    /// Read the event from stdin (accepted explicitly for automation clarity).
    #[arg(long)]
    pub(crate) stdin: bool,

    /// Read a fractal.input.v1 JSON envelope from stdin.
    #[arg(long, conflicts_with = "format")]
    pub(crate) json: bool,

    /// Input mode for text normalization (`fractal-command` or `dictation`).
    #[arg(long, default_value = "fractal-command")]
    pub(crate) mode: String,

    /// Require a human to type the event-specific confirmation on `/dev/tty`.
    #[arg(long)]
    pub(crate) confirm: bool,

    /// Amend the active execution graph; never fall through to a new build.
    #[arg(long, conflicts_with_all = ["confirm", "managed_project"])]
    pub(crate) amend: bool,

    /// Normalize, classify, and print the event without compiling or executing.
    #[arg(long)]
    pub(crate) preview: bool,

    /// Port for the live execution-graph board.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,

    /// Trusted workspace in which the graph should execute (defaults to cwd).
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Create a fresh managed workspace for the signed native macOS companion.
    #[arg(long, hide = true, conflicts_with = "repo")]
    pub(crate) managed_project: bool,

    /// Confirmed display name for a native managed project.
    #[arg(long, hide = true, requires = "managed_project", value_name = "NAME")]
    pub(crate) project_name: Option<String>,

    /// Efficiency governance for the build started by this input event.
    #[command(flatten)]
    pub(crate) efficiency: IngestEfficiencyOpts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum InputFormat {
    Text,
    Json,
}

/// Arguments accepted by `fractal voice` and `fractal dictate`.
#[derive(Debug, Args)]
pub(crate) struct VoiceArgs {
    /// Voice backend. Moonshine runs locally and is the default.
    #[arg(long, value_enum, default_value_t = VoiceEngine::Moonshine)]
    pub(crate) engine: VoiceEngine,

    /// Set up Moonshine or show installed voice-engine status.
    #[command(subcommand)]
    pub(crate) command: Option<VoiceCommand>,

    /// Superwhisper mode key. Falls back to the matching FRACTAL_SUPERWHISPER_* env.
    #[arg(long)]
    pub(crate) mode_key: Option<String>,

    /// Superwhisper delay between selecting the mode and starting recording.
    #[arg(long, default_value_t = 200)]
    pub(crate) delay_ms: u64,

    /// Show what would run without recording or launching another application.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Normalize and print the Moonshine transcript without executing it.
    #[arg(long)]
    pub(crate) preview: bool,

    /// Permit a typed confirmation prompt for non-read-only voice commands.
    #[arg(long)]
    pub(crate) confirm: bool,

    /// Trusted project workspace for a Moonshine command.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Port for the execution graph launched by a Moonshine command.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum VoiceEngine {
    Moonshine,
    Superwhisper,
}

impl std::fmt::Display for VoiceEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Moonshine => "moonshine",
            Self::Superwhisper => "superwhisper",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum VoiceCommand {
    /// Install the isolated Moonshine runtime and Medium Streaming model.
    Setup,
    /// Show availability and selection for every supported voice backend.
    Engines,
}

/// Submission mode understood by the CLI skeleton.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Mode {
    Plan,
    Build,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Plan => "plan",
            Self::Build => "build",
        })
    }
}

/// Worker-provider preference understood by the CLI skeleton.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Provider {
    Auto,
    Cursor,
    Codex,
    Claude,
    Local,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Local => "local",
        })
    }
}

/// Arguments accepted by `fractal graph`.
#[derive(Debug, Args)]
pub(crate) struct GraphArgs {
    /// Board operation to perform.
    #[command(subcommand)]
    pub(crate) command: GraphCommand,
}

/// Live-board operations.
#[derive(Debug, Subcommand)]
pub(crate) enum GraphCommand {
    /// Open the board in the default macOS browser.
    Open,
    /// Serve a committed execution graph on its own board.
    Board(GraphBoardArgs),
    /// Fetch and summarize the board's graph API.
    Status(GraphStatusArgs),
    /// Load a committed execution graph from the local store.
    Show(GraphShowArgs),
    /// Compile a bounded PRD task range into a canonical child graph.
    PlanPrd(GraphPlanPrdArgs),
    /// Audit projects from a frozen repository inventory shard.
    Audit(GraphAuditArgs),
    /// Compose a read-only master graph from a frozen repository inventory.
    Compose(GraphComposeArgs),
    /// Reconcile the six-repository master graph from frozen inventory and audit evidence.
    Reconcile(GraphReconcileArgs),
    /// Serve a read-only master graph from a frozen repository inventory.
    Master(GraphMasterArgs),
    /// Import a legacy graph-state JSON file into .fractal/project.fractal once.
    ImportLegacy(GraphImportLegacyArgs),
    /// Seed a disposable project with a broad parallel graph for join testing.
    SeedParallelTest(GraphSeedParallelTestArgs),
    /// Internal foreground Rust board server.
    #[command(hide = true)]
    Serve(GraphServeArgs),
}

/// Arguments accepted by `fractal graph board`.
#[derive(Debug, Args)]
pub(crate) struct GraphBoardArgs {
    /// Content hash of the committed execution graph.
    pub(crate) graph_hash: String,

    /// Port on which to serve the graph board.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,

    /// Directory containing the execution-graph viewer's server.py.
    #[arg(long, value_name = "PATH")]
    pub(crate) exec_graph_dir: Option<PathBuf>,

    /// Do not open the served board in the default macOS browser.
    #[arg(long)]
    pub(crate) no_open: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShardSpec {
    pub(crate) index: u32,
    pub(crate) total: u32,
}

impl std::str::FromStr for ShardSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((index, total)) = value.split_once('/') else {
            return Err("shard must be INDEX/TOTAL".to_owned());
        };
        if index.is_empty() || total.is_empty() || value.matches('/').count() != 1 {
            return Err("shard must be INDEX/TOTAL".to_owned());
        }
        if index.starts_with('+')
            || total.starts_with('+')
            || index.starts_with('-')
            || total.starts_with('-')
        {
            return Err("shard values must be unsigned decimal integers".to_owned());
        }
        let index: u32 = index
            .parse()
            .map_err(|_| "shard index must be an unsigned decimal integer".to_owned())?;
        let total: u32 = total
            .parse()
            .map_err(|_| "shard total must be an unsigned decimal integer".to_owned())?;
        if total == 0 {
            return Err("shard total must be greater than zero".to_owned());
        }
        if index >= total {
            return Err("shard index must be less than shard total".to_owned());
        }
        Ok(Self { index, total })
    }
}

/// Arguments accepted by `fractal graph audit`.
#[derive(Debug, Args)]
pub(crate) struct GraphAuditArgs {
    /// Frozen fractal.repository_inventory.v1 JSON artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) inventory: PathBuf,

    /// Inventory shard in strict INDEX/TOTAL form; INDEX is zero-based.
    #[arg(long, value_name = "INDEX/TOTAL")]
    pub(crate) shard: ShardSpec,

    /// Run bounded native test commands for audited workspaces.
    #[arg(long)]
    pub(crate) run_tests: bool,

    /// Write the stable JSON audit report to this path.
    #[arg(long, value_name = "PATH")]
    pub(crate) report: PathBuf,
}

/// Arguments accepted by `fractal graph compose`.
#[derive(Debug, Args)]
pub(crate) struct GraphComposeArgs {
    /// Frozen fractal.repository_inventory.v1 JSON artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) inventory: PathBuf,

    /// Print the composed graph or validation summary as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Validate composition only and print a validation summary.
    #[arg(long)]
    pub(crate) validate_only: bool,
}

/// Arguments accepted by `fractal graph reconcile`.
#[derive(Debug, Args)]
pub(crate) struct GraphReconcileArgs {
    /// Frozen fractal.repository_inventory.v1 JSON artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) inventory: PathBuf,

    /// Current graph-audit report (repeat for multiple audit evidence files).
    #[arg(
        long = "audit",
        alias = "current-audit",
        value_name = "PATH",
        required = true
    )]
    pub(crate) audits: Vec<PathBuf>,

    /// Optional prior reconciliation JSON used as the drift baseline.
    #[arg(long, value_name = "PATH")]
    pub(crate) baseline: Option<PathBuf>,

    /// Write canonical reconciliation JSON to this path; stdout when omitted.
    #[arg(long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,
}

/// Arguments accepted by `fractal graph master`.
#[derive(Debug, Args)]
pub(crate) struct GraphMasterArgs {
    /// Frozen fractal.repository_inventory.v1 JSON artifact.
    #[arg(long, value_name = "PATH")]
    pub(crate) inventory: PathBuf,

    /// Port on which to serve the read-only master board.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,

    /// Do not open the served board in the default macOS browser.
    #[arg(long)]
    pub(crate) no_open: bool,
}

#[derive(Debug, Args)]
pub(crate) struct GraphImportLegacyArgs {
    /// Legacy graph-state.json or graph-state-*.json file.
    #[arg(long, value_name = "PATH")]
    pub(crate) state: PathBuf,

    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct GraphSeedParallelTestArgs {
    /// Empty or new project workspace to initialize.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// Number of independent test tasks across all waves.
    #[arg(long, default_value_t = 36, value_parser = clap::value_parser!(u32).range(8..=96))]
    pub(crate) nodes: u32,

    /// Number of parallel lanes available in the first wave.
    #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u32).range(2..=32))]
    pub(crate) first_wave: u32,

    /// Human-readable project title.
    #[arg(long, default_value = "Parallel Join Stress Test")]
    pub(crate) title: String,
}

#[derive(Debug, Args)]
pub(crate) struct GraphServeArgs {
    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// Port on which to serve the board.
    #[arg(long, default_value_t = DEFAULT_GRAPH_PORT)]
    pub(crate) port: u16,

    /// Directory containing the board's static frontend assets.
    #[arg(long, value_name = "PATH")]
    pub(crate) exec_graph_dir: Option<PathBuf>,
}

/// Arguments accepted by `fractal graph show`.
#[derive(Debug, Args)]
pub(crate) struct GraphShowArgs {
    /// Content hash of the committed execution graph.
    pub(crate) graph_hash: String,

    /// Print the complete stored execution graph as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments accepted by `fractal graph plan-prd`.
#[derive(Debug, Args)]
pub(crate) struct GraphPlanPrdArgs {
    /// Project workspace containing `.fractal/project.fractal`.
    #[arg(long, required = true, value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// PRD Markdown path relative to the project workspace.
    #[arg(long, required = true, value_name = "RELATIVE_PATH")]
    pub(crate) prd: PathBuf,

    /// First inclusive PRD task identifier.
    #[arg(long, required = true, value_name = "INT-NNN")]
    pub(crate) from: String,

    /// Last inclusive PRD task identifier.
    #[arg(long, required = true, value_name = "INT-NNN")]
    pub(crate) through: String,

    /// Expected current project graph hash.
    #[arg(long = "expected-parent-hash", required = true, value_name = "SHA256")]
    pub(crate) expected_parent_hash: String,

    /// Commit the compiled child graph and repoint the project at it.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Print a stable JSON report instead of human-readable diagnostics.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments accepted by `fractal graph status`.
#[derive(Debug, Args)]
pub(crate) struct GraphStatusArgs {
    /// Print the API response as JSON instead of a summary.
    #[arg(long)]
    pub(crate) json: bool,

    /// Base URL of the execution-graph board.
    #[arg(long, default_value = DEFAULT_BOARD_URL, value_name = "URL")]
    pub(crate) url: String,
}

/// Arguments accepted by the Coordinate runner.
#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Work identifier to run (legacy stub path when `--graph` is absent).
    #[arg(long, value_name = "ID")]
    pub(crate) work: Option<String>,

    /// Committed execution-graph hash to run through Coordinate.
    #[arg(long, value_name = "SHA256")]
    pub(crate) graph: Option<String>,

    /// A raw `fractal.execution_graph.v1` JSON file to run (with `--local`).
    #[arg(long, value_name = "PATH")]
    pub(crate) graph_file: Option<PathBuf>,

    /// Execute the graph in-process with the local multi-agent team (each agent
    /// checks out ready nodes) instead of enqueuing through Coordinate.
    #[arg(long)]
    pub(crate) local: bool,

    /// Run worker nodes in isolated Git worktrees, then let Fractal integrate
    /// their commits serially before trusted verification.
    #[arg(long, requires = "local")]
    pub(crate) hybrid: bool,

    /// Coordinate SQLite store (defaults to `$FRACTAL_HOME/coordinate.sqlite3`).
    #[arg(long, value_name = "PATH")]
    pub(crate) db: Option<PathBuf>,

    /// The Coordinate `squad` binary (defaults to `$SQUAD_BIN` or `squad`).
    #[arg(long, value_name = "PATH")]
    pub(crate) squad_bin: Option<PathBuf>,

    /// Keep reconciling until every node completes (Coordinate `--watch`).
    #[arg(long)]
    pub(crate) watch: bool,

    /// Print the Coordinate invocation without running it.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Efficiency governance for this run (observation and reporting only;
    /// scheduler mutation is not wired yet).
    #[command(flatten)]
    pub(crate) efficiency: EfficiencyOpts,
}

/// Governed execution-efficiency operating mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum EfficiencyModeArg {
    /// Record efficiency signals only; never propose or apply interventions.
    Observe,
    /// Propose interventions; every one requires explicit approval (default).
    #[default]
    Suggest,
    /// Apply low-impact interventions autonomously; high-impact actions still
    /// require per-action `--allow-high-impact` grants.
    #[value(alias = "auto_optimize")]
    AutoOptimize,
}

impl std::fmt::Display for EfficiencyModeArg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Observe => "observe",
            Self::Suggest => "suggest",
            Self::AutoOptimize => "auto-optimize",
        })
    }
}

/// Repair actions addressable from the command line (contract names accepted
/// as aliases, e.g. `delay_verification`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum InterventionArg {
    Merge,
    Cancel,
    #[value(alias = "delay_verification")]
    DelayVerification,
    #[value(alias = "stop_downstream")]
    StopDownstream,
    Reassign,
    #[value(alias = "consolidate_verifiers")]
    ConsolidateVerifiers,
    #[value(alias = "split_drift")]
    SplitDrift,
}

/// Efficiency governance controls shared by `fractal efficiency` and
/// `fractal run`.
#[derive(Debug, Args)]
pub(crate) struct EfficiencyOpts {
    /// Efficiency operating mode.
    #[arg(
        long = "efficiency-mode",
        visible_alias = "mode",
        value_enum,
        value_name = "MODE",
        default_value_t = EfficiencyModeArg::Suggest
    )]
    pub(crate) efficiency_mode: EfficiencyModeArg,

    /// Explicitly approve a proposed intervention (repeatable).
    #[arg(long = "approve-intervention", value_enum, value_name = "ACTION")]
    pub(crate) approve_intervention: Vec<InterventionArg>,

    /// Explicitly override (decline) a proposed intervention (repeatable).
    #[arg(long = "override-intervention", value_enum, value_name = "ACTION")]
    pub(crate) override_intervention: Vec<InterventionArg>,

    /// Grant scoped autonomy for one named high-impact action (repeatable;
    /// valid only with `--efficiency-mode auto-optimize`).
    #[arg(long = "allow-high-impact", value_enum, value_name = "ACTION")]
    pub(crate) allow_high_impact: Vec<InterventionArg>,
}

/// Ingest efficiency controls omit the `--mode` shorthand because ingest
/// already uses that flag for transcript normalization.
#[derive(Debug, Args)]
pub(crate) struct IngestEfficiencyOpts {
    #[arg(
        long = "efficiency-mode",
        value_enum,
        value_name = "MODE",
        default_value_t = EfficiencyModeArg::Suggest
    )]
    pub(crate) efficiency_mode: EfficiencyModeArg,

    #[arg(long = "approve-intervention", value_enum, value_name = "ACTION")]
    pub(crate) approve_intervention: Vec<InterventionArg>,

    #[arg(long = "override-intervention", value_enum, value_name = "ACTION")]
    pub(crate) override_intervention: Vec<InterventionArg>,

    #[arg(long = "allow-high-impact", value_enum, value_name = "ACTION")]
    pub(crate) allow_high_impact: Vec<InterventionArg>,
}

/// Arguments accepted by `fractal efficiency`.
#[derive(Debug, Args)]
pub(crate) struct EfficiencyArgs {
    #[command(flatten)]
    pub(crate) controls: EfficiencyOpts,

    /// Workspace whose recorded efficiency data should be summarized
    /// (defaults to the current directory).
    #[arg(long, value_name = "PATH")]
    pub(crate) repo: Option<PathBuf>,

    /// Print the resolved configuration and status as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments accepted by the morphogenesis evolve loop (P4.7).
#[derive(Debug, Args)]
pub(crate) struct EvolveArgs {
    /// Evaluate morphogens once and exit.
    #[arg(long, conflicts_with = "watch")]
    pub(crate) once: bool,

    /// Watch the graph and evaluate morphogens continuously.
    #[arg(long)]
    pub(crate) watch: bool,

    /// Milliseconds between watch ticks (default 1000).
    #[arg(long, default_value_t = 1000)]
    pub(crate) interval_ms: u64,

    /// Stop after this many watch ticks (omit for unbounded watch).
    #[arg(long)]
    pub(crate) max_ticks: Option<u64>,

    /// Emit machine-readable JSON tick lines.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Rust-owned execution-node controls.
#[derive(Debug, Args)]
pub(crate) struct NodeArgs {
    /// Stable graph-node identifier.
    pub(crate) id: String,

    /// Show the node's current canonical assignment.
    #[arg(long, visible_alias = "status", conflicts_with_all = ["retry", "cancel", "checkout", "complete", "release"])]
    pub(crate) show: bool,

    /// Retry the node.
    #[arg(long, conflicts_with_all = ["cancel", "checkout", "complete", "release"])]
    pub(crate) retry: bool,

    /// Cancel the node.
    #[arg(long, conflicts_with_all = ["checkout", "complete", "release"])]
    pub(crate) cancel: bool,

    /// Atomically claim a dependency-ready node.
    #[arg(long, conflicts_with_all = ["complete", "release"])]
    pub(crate) checkout: bool,

    /// Complete a node owned by this agent.
    #[arg(long, conflicts_with = "release")]
    pub(crate) complete: bool,

    /// Release a node owned by this agent.
    #[arg(long)]
    pub(crate) release: bool,

    /// Project workspace containing .fractal/project.fractal.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// Stable agent identity (defaults to FRACTAL_AGENT_ID).
    #[arg(long, env = "FRACTAL_AGENT_ID", default_value = "codex/root")]
    pub(crate) agent_id: String,

    /// Human-readable agent label (defaults to FRACTAL_AGENT_LABEL).
    #[arg(long, env = "FRACTAL_AGENT_LABEL", default_value = "Codex")]
    pub(crate) agent_label: String,
}

/// Arguments accepted by `fractal join`.
#[derive(Debug, Args)]
pub(crate) struct JoinArgs {
    /// Agent role requested from the coordinator.
    #[arg(long, default_value = "worker")]
    pub(crate) role: String,

    /// Project workspace, or any directory below it.
    #[arg(long, default_value = ".", value_name = "PATH")]
    pub(crate) repo: PathBuf,

    /// Stable worker identity (defaults to FRACTAL_AGENT_ID or an auto-generated id).
    #[arg(long, env = "FRACTAL_AGENT_ID")]
    pub(crate) agent_id: Option<String>,

    /// Human-readable worker label (defaults to FRACTAL_AGENT_LABEL or an automatic label).
    #[arg(long, env = "FRACTAL_AGENT_LABEL")]
    pub(crate) agent_label: Option<String>,

    /// Emit versioned JSON state records instead of human-readable output.
    #[arg(long)]
    pub(crate) json: bool,

    /// Poll once and return `no_work` when no assignment arrives.
    #[arg(long)]
    pub(crate) once: bool,

    /// Seconds spent waiting in each coordinator receive call.
    #[arg(long, default_value_t = 5)]
    pub(crate) poll_secs: u64,

    /// Maximum total wait time. Zero means wait indefinitely.
    #[arg(long, default_value_t = 86_400)]
    pub(crate) timeout_secs: u64,

    /// Worker assignment lease duration in seconds (positive, capped).
    #[arg(long, default_value_t = 60, env = "FRACTAL_JOIN_LEASE_SECS")]
    pub(crate) lease_secs: u64,

    /// Optional squad executable path. The client/provider is intentionally not selected here.
    #[arg(long, env = "SQUAD_BIN", value_name = "PATH")]
    pub(crate) squad_bin: Option<PathBuf>,
}

/// Arguments for the local coordinator assignment loop.
#[derive(Debug, Args)]
pub(crate) struct CoordinatorArgs {
    /// Project workspace containing `.fractal/project.fractal`.
    #[arg(long, default_value = ".")]
    pub(crate) repo: PathBuf,

    /// Polling interval for worker readiness messages.
    #[arg(long, default_value_t = 2)]
    pub(crate) poll_secs: u64,

    /// Stop after one polling pass.
    #[arg(long)]
    pub(crate) once: bool,

    /// Assignment lease duration in seconds (positive, capped).
    #[arg(long, default_value_t = 60, env = "FRACTAL_JOIN_LEASE_SECS")]
    pub(crate) lease_secs: u64,

    /// Optional squad executable override.
    #[arg(long, env = "SQUAD_BIN")]
    pub(crate) squad_bin: Option<PathBuf>,

    /// Keep coordinator startup diagnostics off the worker's JSON stdout.
    #[arg(long, hide = true)]
    pub(crate) quiet: bool,
}

/// Arguments for the hierarchical specialist-team architect.
#[derive(Debug, Args)]
pub(crate) struct ArchitectArgs {
    /// Project workspace containing `.fractal/project.fractal`.
    #[arg(long, default_value = ".")]
    pub(crate) repo: PathBuf,

    /// Form at most this many teams; zero means no policy cap.
    #[arg(long, default_value_t = 0)]
    pub(crate) max_teams: usize,

    /// Seconds between admission checks.
    #[arg(long, default_value_t = 10)]
    pub(crate) poll_secs: u64,

    /// Refuse a new team above this one-minute-load/logical-core ratio.
    #[arg(long, default_value_t = 1.25)]
    pub(crate) max_load_per_core: f64,

    /// Refuse a new team below this amount of available memory.
    #[arg(long, default_value_t = 8.0)]
    pub(crate) min_free_memory_gib: f64,

    /// Required measured improvement over the accepted baseline, in basis points.
    #[arg(long, default_value_t = 0)]
    pub(crate) min_improvement_bps: i64,

    /// Evaluate one admission cycle and exit.
    #[arg(long)]
    pub(crate) once: bool,

    /// Launch the planned Codex leader and workers; otherwise preview only.
    #[arg(long)]
    pub(crate) launch: bool,

    /// Persist a stop request for the running architect loop.
    #[arg(long, conflicts_with = "launch")]
    pub(crate) stop: bool,

    /// Emit a versioned JSON status record.
    #[arg(long)]
    pub(crate) json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn parses_bare_request() {
        let cli = Cli::try_parse_from(["fractal", "build a tiny CLI"]).unwrap();
        assert_eq!(cli.request.as_deref(), Some("build a tiny CLI"));
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_explicit_offline_interactive_mode() {
        let cli = Cli::try_parse_from(["fractal", "--offline"]).unwrap();
        assert!(cli.offline);
        assert!(cli.request.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_submit_options() {
        let cli = Cli::try_parse_from([
            "fractal",
            "submit",
            "build it",
            "--mode",
            "plan",
            "--provider",
            "cursor",
            "--repo",
            "/tmp/project",
            "--fractalwork",
            "/tmp/fractalwork",
        ])
        .unwrap();
        let Some(Command::Submit(args)) = cli.command else {
            panic!("expected submit command");
        };
        assert_eq!(args.request, "build it");
        assert_eq!(args.mode, Some(Mode::Plan));
        assert_eq!(args.provider, Some(Provider::Cursor));
        assert_eq!(args.repo, Some(PathBuf::from("/tmp/project")));
        assert_eq!(args.port, DEFAULT_GRAPH_PORT);
        assert!(!args.no_open);
        assert_eq!(cli.fractalwork, Some(PathBuf::from("/tmp/fractalwork")));
    }

    #[test]
    fn parses_submit_no_open_and_port() {
        let cli = Cli::try_parse_from([
            "fractal",
            "submit",
            "build it",
            "--mode",
            "build",
            "--no-open",
            "--port",
            "9100",
        ])
        .unwrap();
        let Some(Command::Submit(args)) = cli.command else {
            panic!("expected submit command");
        };
        assert!(args.no_open);
        assert_eq!(args.port, 9100);
    }

    #[test]
    fn parses_graph_commands() {
        let open = Cli::try_parse_from(["fractal", "graph", "open"]).unwrap();
        assert!(matches!(
            open.command,
            Some(Command::Graph(GraphArgs {
                command: GraphCommand::Open
            }))
        ));

        let status = Cli::try_parse_from([
            "fractal",
            "graph",
            "status",
            "--json",
            "--url",
            "http://localhost:9000",
        ])
        .unwrap();
        let Some(Command::Graph(GraphArgs {
            command: GraphCommand::Status(args),
        })) = status.command
        else {
            panic!("expected graph status command");
        };
        assert!(args.json);
        assert_eq!(args.url, "http://localhost:9000");

        let show = Cli::try_parse_from([
            "fractal",
            "graph",
            "show",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--json",
        ])
        .unwrap();
        let Some(Command::Graph(GraphArgs {
            command: GraphCommand::Show(args),
        })) = show.command
        else {
            panic!("expected graph show command");
        };
        assert!(args.json);
        assert!(args.graph_hash.starts_with("sha256:"));

        let plan = Cli::try_parse_from([
            "fractal",
            "graph",
            "plan-prd",
            "--repo",
            "/tmp/project",
            "--prd",
            "docs/implementation.md",
            "--from",
            "INT-008",
            "--through",
            "INT-061",
            "--expected-parent-hash",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--yes",
            "--json",
        ])
        .unwrap();
        let Some(Command::Graph(GraphArgs {
            command: GraphCommand::PlanPrd(args),
        })) = plan.command
        else {
            panic!("expected graph plan-prd command");
        };
        assert_eq!(args.repo, PathBuf::from("/tmp/project"));
        assert_eq!(args.prd, PathBuf::from("docs/implementation.md"));
        assert_eq!(args.from, "INT-008");
        assert_eq!(args.through, "INT-061");
        assert!(args.yes);
        assert!(args.json);

        let board = Cli::try_parse_from([
            "fractal",
            "graph",
            "board",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--port",
            "9002",
            "--exec-graph-dir",
            "/tmp/execution-graph",
            "--no-open",
        ])
        .unwrap();
        let Some(Command::Graph(GraphArgs {
            command: GraphCommand::Board(args),
        })) = board.command
        else {
            panic!("expected graph board command");
        };
        assert_eq!(args.port, 9002);
        assert_eq!(
            args.exec_graph_dir,
            Some(PathBuf::from("/tmp/execution-graph"))
        );
        assert!(args.no_open);
        assert!(args.graph_hash.starts_with("sha256:"));

        let import = Cli::try_parse_from([
            "fractal",
            "graph",
            "import-legacy",
            "--state",
            "graph-state.json",
            "--repo",
            "/tmp/project",
        ])
        .unwrap();
        assert!(matches!(
            import.command,
            Some(Command::Graph(GraphArgs {
                command: GraphCommand::ImportLegacy(GraphImportLegacyArgs { .. })
            }))
        ));

        let audit = Cli::try_parse_from([
            "fractal",
            "graph",
            "audit",
            "--inventory",
            "/tmp/inventory.json",
            "--shard",
            "2/5",
            "--run-tests",
            "--report",
            "/tmp/report.json",
        ])
        .unwrap();
        let Some(Command::Graph(GraphArgs {
            command: GraphCommand::Audit(args),
        })) = audit.command
        else {
            panic!("expected graph audit command");
        };
        assert_eq!(args.inventory, PathBuf::from("/tmp/inventory.json"));
        assert_eq!(args.shard, ShardSpec { index: 2, total: 5 });
        assert!(args.run_tests);
        assert_eq!(args.report, PathBuf::from("/tmp/report.json"));

        let compose = Cli::try_parse_from([
            "fractal",
            "graph",
            "compose",
            "--inventory",
            "/tmp/inventory.json",
            "--json",
            "--validate-only",
        ])
        .unwrap();
        assert!(matches!(
            compose.command,
            Some(Command::Graph(GraphArgs {
                command: GraphCommand::Compose(GraphComposeArgs {
                    inventory,
                    json: true,
                    validate_only: true,
                })
            })) if inventory == std::path::Path::new("/tmp/inventory.json")
        ));
    }

    #[test]
    fn rejects_malformed_graph_audit_shards() {
        for shard in ["1", "1/0", "2/2", "-1/2", "+1/2", "1/2/3", "a/2"] {
            assert!(
                Cli::try_parse_from([
                    "fractal",
                    "graph",
                    "audit",
                    "--inventory",
                    "/tmp/inventory.json",
                    "--shard",
                    shard,
                    "--report",
                    "/tmp/report.json",
                ])
                .is_err(),
                "shard {shard:?} should fail"
            );
        }
    }

    #[test]
    fn parses_gate_preview_tokens_for_record_and_revoke() {
        let token = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let record = Cli::try_parse_from([
            "fractal",
            "gate",
            "record",
            "--node",
            "secure",
            "--gate",
            "security_review",
            "--evidence",
            "review.txt",
            "--reviewer-id",
            "reviewer",
            "--role",
            "security_reviewer",
            "--attestation",
            "approve:graph:secure:security_review",
            "--yes",
            "--expected-content-hash",
            token,
        ])
        .unwrap();
        let Some(Command::Gate(GateArgs {
            command: GateCommand::Record(args),
        })) = record.command
        else {
            panic!("expected gate record command");
        };
        assert!(args.yes);
        assert_eq!(args.expected_content_hash.as_deref(), Some(token));

        let revoke = Cli::try_parse_from([
            "fractal",
            "gate",
            "revoke",
            "--approval-hash",
            token,
            "--reviewer-id",
            "revoker",
            "--role",
            "security_reviewer",
            "--attestation",
            "revoke:graph:secure:security_review:approval",
            "--yes",
            "--expected-content-hash",
            token,
        ])
        .unwrap();
        let Some(Command::Gate(GateArgs {
            command: GateCommand::Revoke(args),
        })) = revoke.command
        else {
            panic!("expected gate revoke command");
        };
        assert!(args.yes);
        assert_eq!(args.expected_content_hash.as_deref(), Some(token));
    }

    #[test]
    fn parses_run_evolve_node_and_version() {
        let run = Cli::try_parse_from(["fractal", "run", "--work", "work-7"]).unwrap();
        assert!(matches!(
            run.command,
            Some(Command::Run(RunArgs { work: Some(ref id), .. })) if id == "work-7"
        ));

        let evolve = Cli::try_parse_from(["fractal", "evolve", "--watch"]).unwrap();
        assert!(matches!(
            evolve.command,
            Some(Command::Evolve(EvolveArgs { watch: true, .. }))
        ));

        let node = Cli::try_parse_from(["fractal", "node", "P2.3", "--retry"]).unwrap();
        assert!(matches!(
            node.command,
            Some(Command::Node(NodeArgs { retry: true, .. }))
        ));
        let checkout = Cli::try_parse_from([
            "fractal",
            "node",
            "P2.3",
            "--checkout",
            "--repo",
            "/tmp/project",
            "--agent-id",
            "codex/root",
            "--agent-label",
            "Codex",
        ])
        .unwrap();
        assert!(matches!(
            checkout.command,
            Some(Command::Node(NodeArgs { checkout: true, .. }))
        ));

        let version = Cli::try_parse_from(["fractal", "version"]).unwrap();
        assert!(matches!(version.command, Some(Command::Version)));
    }

    #[test]
    fn parses_worker_join_with_worker_default_and_no_client_selector() {
        let previous_id = std::env::var_os("FRACTAL_AGENT_ID");
        let previous_label = std::env::var_os("FRACTAL_AGENT_LABEL");
        let previous_lease = std::env::var_os("FRACTAL_JOIN_LEASE_SECS");
        std::env::remove_var("FRACTAL_AGENT_ID");
        std::env::remove_var("FRACTAL_AGENT_LABEL");
        std::env::remove_var("FRACTAL_JOIN_LEASE_SECS");
        let join = Cli::try_parse_from(["fractal", "join", "--role", "worker", "--once", "--json"])
            .unwrap();
        let Some(Command::Join(args)) = join.command else {
            panic!("expected join command");
        };
        assert_eq!(args.role, "worker");
        assert!(args.once);
        assert!(args.json);
        assert_eq!(
            args.agent_id.as_deref(),
            std::env::var("FRACTAL_AGENT_ID").ok().as_deref()
        );
        assert_eq!(
            args.agent_label.as_deref(),
            std::env::var("FRACTAL_AGENT_LABEL").ok().as_deref()
        );
        assert_eq!(args.lease_secs, 60);
        restore_env("FRACTAL_AGENT_ID", previous_id);
        restore_env("FRACTAL_AGENT_LABEL", previous_label);
        restore_env("FRACTAL_JOIN_LEASE_SECS", previous_lease);
    }

    #[test]
    fn parses_worker_join_and_coordinator_lease_secs() {
        let previous_lease = std::env::var_os("FRACTAL_JOIN_LEASE_SECS");
        std::env::remove_var("FRACTAL_JOIN_LEASE_SECS");
        let join =
            Cli::try_parse_from(["fractal", "join", "--role", "worker", "--lease-secs", "90"])
                .unwrap();
        let Some(Command::Join(args)) = join.command else {
            panic!("expected join command");
        };
        assert_eq!(args.lease_secs, 90);

        let coordinator =
            Cli::try_parse_from(["fractal", "coordinator", "--lease-secs", "120"]).unwrap();
        let Some(Command::Coordinator(args)) = coordinator.command else {
            panic!("expected coordinator command");
        };
        assert_eq!(args.lease_secs, 120);
        restore_env("FRACTAL_JOIN_LEASE_SECS", previous_lease);
    }

    #[test]
    fn parses_hierarchical_architect_controls() {
        let cli = Cli::try_parse_from([
            "fractal",
            "architect",
            "--repo",
            "/tmp/project",
            "--max-teams",
            "7",
            "--max-load-per-core",
            "1.5",
            "--min-free-memory-gib",
            "12",
            "--launch",
            "--once",
            "--json",
        ])
        .unwrap();
        let Some(Command::Architect(args)) = cli.command else {
            panic!("expected architect command");
        };
        assert_eq!(args.max_teams, 7);
        assert_eq!(args.max_load_per_core, 1.5);
        assert_eq!(args.min_free_memory_gib, 12.0);
        assert!(args.launch && args.once && args.json);
        assert!(Cli::try_parse_from(["fractal", "architect", "--launch", "--stop"]).is_err());
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn parses_ingest_and_voice_commands() {
        let ingest = Cli::try_parse_from([
            "fractal",
            "ingest",
            "--source",
            "superwhisper",
            "--format",
            "text",
            "--stdin",
            "--preview",
        ])
        .unwrap();
        let Some(Command::Ingest(args)) = ingest.command else {
            panic!("expected ingest command");
        };
        assert_eq!(args.source, "superwhisper");
        assert_eq!(args.format, InputFormat::Text);
        assert!(args.stdin);
        assert!(args.preview);

        let amendment = Cli::try_parse_from(["fractal", "ingest", "--stdin", "--amend"]).unwrap();
        assert!(matches!(
            amendment.command,
            Some(Command::Ingest(IngestArgs { amend: true, .. }))
        ));
        assert!(
            Cli::try_parse_from(["fractal", "ingest", "--stdin", "--amend", "--confirm"]).is_err()
        );
        let completed_project = Cli::try_parse_from([
            "fractal",
            "ingest",
            "--stdin",
            "--amend",
            "--repo",
            "/tmp/project",
        ])
        .unwrap();
        assert!(matches!(
            completed_project.command,
            Some(Command::Ingest(IngestArgs {
                amend: true,
                repo: Some(ref repo),
                ..
            })) if repo == &PathBuf::from("/tmp/project")
        ));

        let json = Cli::try_parse_from(["fractal", "ingest", "--json"]).unwrap();
        assert!(matches!(
            json.command,
            Some(Command::Ingest(IngestArgs { json: true, .. }))
        ));

        let voice = Cli::try_parse_from([
            "fractal",
            "voice",
            "--engine",
            "superwhisper",
            "--mode-key",
            "fractal-command",
            "--dry-run",
        ])
        .unwrap();
        assert!(matches!(
            voice.command,
            Some(Command::Voice(VoiceArgs {
                mode_key: Some(ref key),
                engine: VoiceEngine::Superwhisper,
                dry_run: true,
                ..
            })) if key == "fractal-command"
        ));

        let dictate = Cli::try_parse_from(["fractal", "dictate", "--dry-run"]).unwrap();
        assert!(matches!(
            dictate.command,
            Some(Command::Dictate(VoiceArgs {
                engine: VoiceEngine::Moonshine,
                dry_run: true,
                ..
            }))
        ));
        let setup = Cli::try_parse_from(["fractal", "voice", "setup"]).unwrap();
        assert!(matches!(
            setup.command,
            Some(Command::Voice(VoiceArgs {
                command: Some(VoiceCommand::Setup),
                ..
            }))
        ));
        let companion = Cli::try_parse_from([
            "fractal",
            "ingest",
            "--source",
            "fractal-mac-app",
            "--stdin",
            "--managed-project",
            "--project-name",
            "Pocket Ledger",
        ])
        .unwrap();
        assert!(matches!(
            companion.command,
            Some(Command::Ingest(IngestArgs {
                source,
                managed_project: true,
                project_name: Some(project_name),
                ..
            })) if source == "fractal-mac-app" && project_name == "Pocket Ledger"
        ));
    }

    #[test]
    fn parses_run_control_commands() {
        assert!(matches!(
            Cli::try_parse_from(["fractal", "stop"]).unwrap().command,
            Some(Command::Stop(StopArgs {
                project: None,
                all: false
            }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "stop", "--project", "expense-app"])
                .unwrap()
                .command,
            Some(Command::Stop(StopArgs {
                project: Some(ref project),
                all: false
            })) if project == "expense-app"
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "pause", "--project", "expense-app"])
                .unwrap()
                .command,
            Some(Command::Stop(StopArgs {
                project: Some(ref project),
                all: false
            })) if project == "expense-app"
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "stop", "--all"])
                .unwrap()
                .command,
            Some(Command::Stop(StopArgs {
                project: None,
                all: true
            }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["fractal", "status", "--running"])
                .unwrap()
                .command,
            Some(Command::Status(StatusArgs { running: true }))
        ));
        assert!(Cli::try_parse_from(["fractal", "stop", "--all", "--project", "app"]).is_err());
    }

    #[test]
    fn parses_native_and_cross_platform_mobile_commands() {
        let ios = Cli::try_parse_from([
            "fractal",
            "ios",
            "Build a personal expense tracker",
            "--launch",
        ])
        .unwrap();
        assert!(matches!(
            ios.command,
            Some(Command::Ios(IosArgs {
                launch: true,
                ref request,
                ..
            })) if request == "Build a personal expense tracker"
        ));

        let mobile = Cli::try_parse_from([
            "fractal",
            "mobile",
            "Build a personal expense tracker",
            "--framework",
            "expo",
            "--platforms",
            "ios,android",
            "--launch",
            "ios",
        ])
        .unwrap();
        let Some(Command::Mobile(args)) = mobile.command else {
            panic!("expected mobile command");
        };
        assert_eq!(args.framework, MobileFramework::Expo);
        assert_eq!(
            args.platforms,
            vec![MobilePlatform::Ios, MobilePlatform::Android]
        );
        assert_eq!(args.launch, Some(MobilePlatform::Ios));
    }

    #[test]
    fn parses_login_and_sync_commands() {
        let login = Cli::try_parse_from([
            "fractal",
            "login",
            "--server",
            "http://127.0.0.1:3000",
            "--no-open",
            "--timeout",
            "30",
            "--status",
        ])
        .unwrap();
        assert!(matches!(
            login.command,
            Some(Command::Login(LoginArgs {
                no_open: true,
                timeout: 30,
                status: true,
                ..
            }))
        ));

        let sync = Cli::try_parse_from([
            "fractal", "sync", "--enable", "--github", "--repo", "/tmp/app",
        ])
        .unwrap();
        assert!(matches!(
            sync.command,
            Some(Command::Sync(SyncArgs {
                enable: true,
                github: true,
                ..
            }))
        ));
    }

    #[test]
    fn parses_confirmed_email_and_x_help_commands() {
        let invite = Cli::try_parse_from([
            "fractal",
            "invite",
            "--project",
            "coffee-2",
            "--email",
            "helper@example.com",
            "--role",
            "contributor",
            "--message",
            "Task 2.1 and spare compute",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            invite.command,
            Some(Command::Invite(InviteArgs {
                ref project,
                role: InvitationRole::Contributor,
                yes: true,
                ..
            })) if project == "coffee-2"
        ));

        let share = Cli::try_parse_from([
            "fractal",
            "share-x",
            "--project",
            "coffee-2",
            "--handle",
            "@helper",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            share.command,
            Some(Command::ShareX(ShareXArgs {
                ref handle,
                yes: true,
                ..
            })) if handle == "@helper"
        ));

        let fast_preview = Cli::try_parse_from([
            "fractal",
            "share-x",
            "--project",
            "coffee-2",
            "--handle",
            "@helper",
            "--preview-only",
        ])
        .unwrap();
        assert!(matches!(
            fast_preview.command,
            Some(Command::ShareX(ShareXArgs {
                preview_only: true,
                yes: false,
                ..
            }))
        ));

        let connect =
            Cli::try_parse_from(["fractal", "connect-x", "--project", "coffee-2"]).unwrap();
        assert!(matches!(
            connect.command,
            Some(Command::ConnectX(ConnectXArgs {
                project: Some(ref project),
                no_open: false,
                ..
            })) if project == "coffee-2"
        ));
    }

    #[test]
    fn parses_guarded_project_visibility_commands() {
        let preview =
            Cli::try_parse_from(["fractal", "visibility", "--project", "coffee-2", "--public"])
                .unwrap();
        assert!(matches!(
            preview.command,
            Some(Command::Visibility(VisibilityArgs {
                ref project,
                public: true,
                private: false,
                yes: false,
            })) if project == "coffee-2"
        ));
        let confirmed = Cli::try_parse_from([
            "fractal",
            "visibility",
            "--project",
            "coffee-2",
            "--private",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            confirmed.command,
            Some(Command::Visibility(VisibilityArgs {
                private: true,
                yes: true,
                ..
            }))
        ));
    }

    #[test]
    fn parses_bridge_free_native_handoff() {
        let cli = Cli::try_parse_from(["fractal", "handoff", "--name", "Hello World"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Handoff(HandoffArgs {
                ref project_name
            })) if project_name == "Hello World"
        ));
    }

    #[test]
    fn legacy_bridge_is_parseable_only_for_deterministic_migration() {
        let bare = Cli::try_parse_from(["fractal", "bridge"]).unwrap();
        assert!(matches!(bare.command, Some(Command::Bridge(_))));
        for command in ["serve", "install", "token", "status"] {
            let cli = Cli::try_parse_from(["fractal", "bridge", command]).unwrap();
            assert!(matches!(cli.command, Some(Command::Bridge(_))));
        }

        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("bridge"));
        let bridge_help = Cli::try_parse_from(["fractal", "bridge", "--help"])
            .expect_err("bridge help exits through clap");
        let bridge_help = bridge_help.to_string();
        assert!(bridge_help.contains("Deprecated compatibility parser"));
        assert!(!bridge_help.contains("serve"));
        assert!(!bridge_help.contains("install"));
        assert!(!bridge_help.contains("token"));
        assert!(BRIDGE_MIGRATION_MESSAGE.contains("fractal handoff"));
    }

    #[test]
    fn efficiency_defaults_to_suggest_with_no_grants() {
        let cli = Cli::try_parse_from(["fractal", "efficiency"]).unwrap();
        let Some(Command::Efficiency(args)) = cli.command else {
            panic!("expected efficiency command");
        };
        assert_eq!(args.controls.efficiency_mode, EfficiencyModeArg::Suggest);
        assert!(args.controls.approve_intervention.is_empty());
        assert!(args.controls.override_intervention.is_empty());
        assert!(args.controls.allow_high_impact.is_empty());
        assert!(args.repo.is_none());
        assert!(!args.json);
    }

    #[test]
    fn parses_efficiency_modes_and_intervention_input() {
        let cli = Cli::try_parse_from([
            "fractal",
            "efficiency",
            "--mode",
            "auto-optimize",
            "--approve-intervention",
            "merge",
            "--approve-intervention",
            "delay_verification",
            "--override-intervention",
            "split-drift",
            "--allow-high-impact",
            "cancel",
            "--allow-high-impact",
            "stop_downstream",
            "--repo",
            "/tmp/project",
            "--json",
        ])
        .unwrap();
        let Some(Command::Efficiency(args)) = cli.command else {
            panic!("expected efficiency command");
        };
        assert_eq!(
            args.controls.efficiency_mode,
            EfficiencyModeArg::AutoOptimize
        );
        assert_eq!(
            args.controls.approve_intervention,
            vec![InterventionArg::Merge, InterventionArg::DelayVerification]
        );
        assert_eq!(
            args.controls.override_intervention,
            vec![InterventionArg::SplitDrift]
        );
        assert_eq!(
            args.controls.allow_high_impact,
            vec![InterventionArg::Cancel, InterventionArg::StopDownstream]
        );
        assert_eq!(args.repo, Some(PathBuf::from("/tmp/project")));
        assert!(args.json);

        // The contract-style underscore spelling of the mode is accepted too.
        let observe =
            Cli::try_parse_from(["fractal", "efficiency", "--efficiency-mode", "observe"]).unwrap();
        assert!(matches!(
            observe.command,
            Some(Command::Efficiency(EfficiencyArgs {
                controls: EfficiencyOpts {
                    efficiency_mode: EfficiencyModeArg::Observe,
                    ..
                },
                ..
            }))
        ));
        let underscored =
            Cli::try_parse_from(["fractal", "efficiency", "--mode", "auto_optimize"]).unwrap();
        assert!(matches!(
            underscored.command,
            Some(Command::Efficiency(EfficiencyArgs {
                controls: EfficiencyOpts {
                    efficiency_mode: EfficiencyModeArg::AutoOptimize,
                    ..
                },
                ..
            }))
        ));
    }

    #[test]
    fn rejects_unknown_efficiency_values() {
        assert!(Cli::try_parse_from(["fractal", "efficiency", "--mode", "autonomous"]).is_err());
        assert!(Cli::try_parse_from([
            "fractal",
            "efficiency",
            "--approve-intervention",
            "rewrite"
        ])
        .is_err());
    }

    #[test]
    fn run_carries_flattened_efficiency_controls() {
        let run = Cli::try_parse_from([
            "fractal",
            "run",
            "--graph",
            "sha256:0123",
            "--efficiency-mode",
            "observe",
        ])
        .unwrap();
        let Some(Command::Run(args)) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(args.efficiency.efficiency_mode, EfficiencyModeArg::Observe);
        assert!(args.efficiency.allow_high_impact.is_empty());

        let default_run = Cli::try_parse_from(["fractal", "run", "--work", "work-7"]).unwrap();
        let Some(Command::Run(args)) = default_run.command else {
            panic!("expected run command");
        };
        assert_eq!(args.efficiency.efficiency_mode, EfficiencyModeArg::Suggest);

        let hybrid = Cli::try_parse_from([
            "fractal",
            "run",
            "--local",
            "--hybrid",
            "--graph-file",
            "graph.json",
        ])
        .unwrap();
        let Some(Command::Run(args)) = hybrid.command else {
            panic!("expected hybrid run command");
        };
        assert!(args.local && args.hybrid);
        assert!(
            Cli::try_parse_from(["fractal", "run", "--hybrid", "--graph-file", "graph.json"])
                .is_err()
        );
    }

    #[test]
    fn ingest_carries_native_efficiency_controls() {
        let ingest = Cli::try_parse_from([
            "fractal",
            "ingest",
            "--stdin",
            "--efficiency-mode",
            "auto-optimize",
            "--allow-high-impact",
            "stop-downstream",
        ])
        .unwrap();
        let Some(Command::Ingest(args)) = ingest.command else {
            panic!("expected ingest command");
        };
        assert_eq!(
            args.efficiency.efficiency_mode,
            EfficiencyModeArg::AutoOptimize
        );
        assert_eq!(
            args.efficiency.allow_high_impact,
            vec![InterventionArg::StopDownstream]
        );
    }

    #[test]
    fn unknown_nested_command_errors_cleanly() {
        let error = Cli::try_parse_from(["fractal", "graph", "frobnicate"])
            .expect_err("unknown graph command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("unrecognized subcommand"));
    }

    #[test]
    fn parses_harness_validate_and_show_commands() {
        let validate = Cli::try_parse_from([
            "fractal",
            "harness",
            "validate",
            "--repo",
            "/tmp/project",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            validate.command,
            Some(Command::Harness(HarnessArgs {
                command: HarnessCommand::Validate(HarnessPolicyArgs { ref repo, json: true })
            })) if repo == &PathBuf::from("/tmp/project")
        ));

        let show = Cli::try_parse_from(["fractal", "harness", "show"]).unwrap();
        assert!(matches!(
            show.command,
            Some(Command::Harness(HarnessArgs {
                command: HarnessCommand::Show(HarnessPolicyArgs { ref repo, json: false })
            })) if repo == &PathBuf::from(".")
        ));
    }

    #[test]
    fn parses_amendment_list_and_reject_controls() {
        let list = Cli::try_parse_from([
            "fractal",
            "amendment",
            "list",
            "--repo",
            "/tmp/project",
            "--json",
        ])
        .unwrap();
        let Some(Command::Amendment(AmendmentArgs {
            command:
                AmendmentCommand::List(AmendmentListArgs {
                    ref repo,
                    json: true,
                }),
        })) = list.command
        else {
            panic!("expected amendment list command");
        };
        assert_eq!(repo, &PathBuf::from("/tmp/project"));

        let reject = Cli::try_parse_from([
            "fractal",
            "amendment",
            "reject",
            "command-7",
            "--repo",
            "/tmp/project",
            "--reason",
            "stale request",
            "--yes",
            "--json",
        ])
        .unwrap();
        let Some(Command::Amendment(AmendmentArgs {
            command:
                AmendmentCommand::Reject(AmendmentRejectArgs {
                    ref command_id,
                    ref repo,
                    ref reason,
                    yes: true,
                    json: true,
                }),
        })) = reject.command
        else {
            panic!("expected amendment reject command");
        };
        assert_eq!(command_id, "command-7");
        assert_eq!(repo, &PathBuf::from("/tmp/project"));
        assert_eq!(reason, "stale request");
    }

    #[test]
    fn amendment_reject_requires_safe_inputs() {
        assert!(Cli::try_parse_from(["fractal", "amendment", "list"]).is_err());
        assert!(Cli::try_parse_from([
            "fractal",
            "amendment",
            "reject",
            "command-7",
            "--repo",
            "/tmp/project",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "fractal",
            "amendment",
            "reject",
            "command-7",
            "--reason",
            "stale request",
        ])
        .is_err());
    }
}
