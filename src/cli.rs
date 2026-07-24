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
    /// Open or inspect the live execution-graph board.
    Graph(GraphArgs),
    /// Run a compiled graph through Coordinate (stub).
    Run(RunArgs),
    /// Run the graph morphogenesis loop (stub).
    Evolve(EvolveArgs),
    /// Inspect or control one graph node (stub).
    Node(NodeArgs),
    /// Safely clear a fractal workspace/test folder (guarded + confirmed).
    Clean(CleanArgs),
    /// GRPO-train an adapter from accumulated verifiable rewards (fractal-rlvr).
    Train,
    /// Show + verify the durable machine-scale chain of folded run receipts.
    Chain,
    /// Print the Fractal CLI version.
    Version,
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

/// Arguments accepted by `fractal graph show`.
#[derive(Debug, Args)]
pub(crate) struct GraphShowArgs {
    /// Content hash of the committed execution graph.
    pub(crate) graph_hash: String,

    /// Print the complete stored execution graph as JSON.
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

/// Arguments accepted by future per-node controls.
#[derive(Debug, Args)]
pub(crate) struct NodeArgs {
    /// Stable graph-node identifier.
    pub(crate) id: String,

    /// Show the node.
    #[arg(long, conflicts_with_all = ["retry", "cancel"])]
    pub(crate) show: bool,

    /// Retry the node.
    #[arg(long, conflicts_with = "cancel")]
    pub(crate) retry: bool,

    /// Cancel the node.
    #[arg(long)]
    pub(crate) cancel: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_request() {
        let cli = Cli::try_parse_from(["fractal", "build a tiny CLI"]).unwrap();
        assert_eq!(cli.request.as_deref(), Some("build a tiny CLI"));
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

        let version = Cli::try_parse_from(["fractal", "version"]).unwrap();
        assert!(matches!(version.command, Some(Command::Version)));
    }

    #[test]
    fn unknown_nested_command_errors_cleanly() {
        let error = Cli::try_parse_from(["fractal", "graph", "frobnicate"])
            .expect_err("unknown graph command must fail");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("unrecognized subcommand"));
    }
}
