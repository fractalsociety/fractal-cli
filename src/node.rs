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
    crate::project_file::transition(
        &args.repo,
        &args.id,
        action,
        &args.agent_id,
        &args.agent_label,
    )?;
    crate::project_sync::maybe_sync_runtime(&args.repo);
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
    Ok(())
}
