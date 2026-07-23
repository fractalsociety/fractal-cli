//! `fractal evolve --watch` / `--once` wiring (pipeline P4.7).

use anyhow::{bail, Result};
use fractal_evolution::{
    build_morphogen, run_evolve_once, run_evolve_watch, DemoEvolveSource, EvolveTickOutcome,
    EvolveWatchConfig, EvolveWatchSink, MorphogenDiffBounds, MorphogenOperation, MorphogenRegistry,
    MorphogenScale, MorphogenTrigger, SystemEvolveClock,
};

use crate::cli::EvolveArgs;

struct PrintingSink {
    json: bool,
}

impl EvolveWatchSink for PrintingSink {
    fn on_tick(&mut self, outcome: &EvolveTickOutcome) -> bool {
        if self.json {
            match serde_json::to_string(outcome) {
                Ok(line) => println!("{line}"),
                Err(error) => eprintln!("error: failed to serialize evolve tick: {error}"),
            }
        } else {
            let fired = if outcome.fired.is_empty() {
                "(none)".to_owned()
            } else {
                outcome
                    .fired
                    .iter()
                    .map(|fired| format!("{}:{:?}", fired.morphogen_id, fired.operation))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!(
                "tick={} event={:?} graph={} fired=[{fired}]",
                outcome.tick, outcome.event.kind, outcome.graph_hash
            );
        }
        true
    }
}

/// Default morphogens for the evolve watch loop (timer probe + repair-on-fail).
fn default_registry() -> Result<MorphogenRegistry> {
    let mut registry = MorphogenRegistry::new();
    registry.register(build_morphogen(
        "timer.probe",
        MorphogenTrigger {
            kind: "timer".into(),
            predicate: "every watch tick".into(),
        },
        MorphogenOperation::Differentiate,
        MorphogenDiffBounds {
            max_changed_nodes: 1,
            max_changed_edges: 1,
        },
        "probe cold nodes on timer",
        MorphogenScale::Graph,
        vec!["capability-requirements".into()],
    )?)?;
    registry.register(build_morphogen(
        "repair.on_fail",
        MorphogenTrigger {
            kind: "node_failed".into(),
            predicate: "node.state == failed".into(),
        },
        MorphogenOperation::Repair,
        MorphogenDiffBounds {
            max_changed_nodes: 2,
            max_changed_edges: 2,
        },
        "regenerate failed subgraph from last consistent state",
        MorphogenScale::Subgraph,
        vec!["retry-policy".into(), "harness-topology".into()],
    )?)?;
    Ok(registry)
}

/// Run the evolve subcommand (`--once` or `--watch`).
pub(crate) fn run_evolve(args: &EvolveArgs) -> Result<()> {
    if !args.once && !args.watch {
        bail!("specify --once or --watch (see `fractal evolve --help`)");
    }
    if args.watch && args.interval_ms == 0 {
        bail!("--interval-ms must be > 0");
    }

    let registry = default_registry()?;
    let mut source = DemoEvolveSource::default();
    let mut sink = PrintingSink { json: args.json };
    let clock = SystemEvolveClock;

    if args.once {
        println!(
            "Evolution mode: once (demo source, {} morphogens)",
            registry.len()
        );
        let outcome = run_evolve_once(&registry, &mut source, &mut sink, &clock)?;
        println!(
            "Evolve once complete: fired {} morphogen(s)",
            outcome.fired.len()
        );
        return Ok(());
    }

    let config = EvolveWatchConfig {
        interval_ms: args.interval_ms,
        max_ticks: args.max_ticks,
    };
    println!(
        "Evolution mode: watch (interval_ms={}, max_ticks={:?}, {} morphogens)",
        config.interval_ms,
        config.max_ticks,
        registry.len()
    );
    println!("Evaluating morphogen triggers only — governed apply remains P4.6.");
    let ticks = run_evolve_watch(&registry, &mut source, &mut sink, &clock, &config)?;
    println!("Evolve watch stopped after {ticks} tick(s)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    #[test]
    fn parses_watch_options() {
        let cli = Cli::try_parse_from([
            "fractal",
            "evolve",
            "--watch",
            "--interval-ms",
            "50",
            "--max-ticks",
            "3",
            "--json",
        ])
        .expect("parse");
        let Some(Command::Evolve(args)) = cli.command else {
            panic!("expected evolve");
        };
        assert!(args.watch);
        assert_eq!(args.interval_ms, 50);
        assert_eq!(args.max_ticks, Some(3));
        assert!(args.json);
    }

    #[test]
    fn once_runs_without_error() {
        let args = EvolveArgs {
            once: true,
            watch: false,
            interval_ms: 1000,
            max_ticks: None,
            json: true,
        };
        run_evolve(&args).expect("once");
    }

    #[test]
    fn watch_respects_max_ticks() {
        let args = EvolveArgs {
            once: false,
            watch: true,
            interval_ms: 1,
            max_ticks: Some(2),
            json: true,
        };
        run_evolve(&args).expect("watch");
    }
}
