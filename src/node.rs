use anyhow::{bail, Result};

use crate::cli::NodeArgs;

pub(crate) fn run(args: &NodeArgs) -> Result<()> {
    let action_count = [
        args.show,
        args.retry,
        args.cancel,
        args.checkout,
        args.complete,
        args.release,
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count();
    if action_count > 1 {
        bail!("select exactly one node operation");
    }

    if args.show || action_count == 0 {
        let assignment = crate::project_file::assignment(&args.repo, &args.id)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": "fractal.node_status.v1",
                "node_id": args.id,
                "assignment": assignment,
            }))?
        );
        return Ok(());
    }

    let action = if args.checkout || args.retry {
        "checkout"
    } else if args.complete {
        "complete"
    } else if args.release || args.cancel {
        "release"
    } else {
        unreachable!("node operation already validated")
    };
    if action == "checkout"
        && crate::architect::enabled(&args.repo)
        && !crate::architect::checkout_authorized(&args.repo, &args.agent_id, &args.id)
    {
        bail!(
            "architect mode permits agent `{}` to checkout only its leader-assigned team node",
            args.agent_id
        );
    }
    crate::project_file::transition(
        &args.repo,
        &args.id,
        action,
        &args.agent_id,
        &args.agent_label,
    )?;
    let next = if action == "complete" && !crate::architect::enabled(&args.repo) {
        Some(crate::coordinator::checkout_next(
            &args.repo,
            &args.agent_id,
            &args.agent_label,
        ))
    } else {
        None
    };
    if let Err(error) = crate::project_sync::sync_worker_transition_now(&args.repo) {
        eprintln!("  live graph sync note: {error:#}");
    }
    println!(
        "{}: {} by {} ({})",
        match action {
            "checkout" => "Checked out",
            "complete" => "Completed",
            _ => "Released",
        },
        args.id,
        args.agent_label,
        args.agent_id
    );
    if let Some(next) = next {
        match next? {
            crate::coordinator::NextAssignment::Assigned(node_id) => {
                println!(
                    "Next assigned: {node_id} to {} ({})",
                    args.agent_label, args.agent_id
                );
                println!(
                    "Inspect it with: fractal node {node_id} --show --repo {}",
                    args.repo.display()
                );
            }
            crate::coordinator::NextAssignment::GraphComplete => {
                println!("No next task: the execution graph is complete.");
            }
            crate::coordinator::NextAssignment::AmendmentRequested => {
                println!(
                    "No dependency-ready task is available; the coordinator requested governed graph expansion."
                );
            }
        }
    }
    Ok(())
}
